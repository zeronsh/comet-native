//! RegistryClient — WebSocket transport for the workspace registry
//! (docs/registry-sync.md): hello/cursor handshake, push/ack for pending op
//! batches, merged-row broadcasts, presence beats, probe/redial liveness, and
//! reconnect with exponential backoff.
//!
//! The client owns no row semantics: everything applies through the shared
//! [`zeron_doc::RegistryDoc`] under a lock. Wire frames are JSON text —
//! byte-compatible with `edge/src/registry-room.ts`.
//!
//! Liveness discipline is inherited from `room.rs` and its incidents: the
//! transport-level text ping elicits a runtime auto-pong that proves NOTHING
//! about the DO (2026-07-30), so room-level health is judged only by protocol
//! frames — a probe that goes unanswered past its deadline tears the session
//! down for a fresh dial.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use zeron_doc::{PendingBatch, RegistryDoc, RegistryRow, StateOutcome};

use crate::types::{RoomStatsSnapshot, StaticUrl, SyncError, UrlProvider};

/// Text `"ping"` keepalive interval (answered by the DO auto-response pair
/// without waking it — transport liveness only).
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Transport silence lease: pongs count, so a healthy socket never trips this.
const SILENCE_LEASE: Duration = Duration::from_secs(45);
/// Bound on one dial attempt (URL fetch + WS handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The server must answer a hello with `state` within this deadline.
const HELLO_DEADLINE: Duration = Duration::from_secs(15);
/// A probe's `probe-ok` (or any other protocol frame) must arrive within this
/// deadline, or the session is torn down for a fresh socket.
const PROBE_DEADLINE: Duration = Duration::from_secs(10);
/// Presence entries older than this are treated as expired (mirrors the
/// EphemeralStore's 30s TTL the old workspace room used).
const PRESENCE_TTL: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Worst-case dark window after the network returns (event wakes usually
/// beat this; the cap only matters when every event path missed).
const BACKOFF_CAP: Duration = Duration::from_secs(16);
/// A joined session must survive this long before a disconnect resets the
/// backoff to base. Resetting on join alone lets a connect-and-die socket
/// (captive portal, half-broken NAT) hot-loop at 250ms forever.
const STABLE_RESET: Duration = Duration::from_secs(30);
/// Safety re-check cadence while parked on "OS says offline" — a stuck or
/// wrong path monitor degrades to slow polling, never to silence.
const OFFLINE_PARK_RECHECK: Duration = Duration::from_secs(30);

/// Quiet-room probe cadence default. The workspace registry is one room per
/// engine, so a fixed 15min cadence costs ~100 DO wakes/day total.
const PROBE_QUIET_DEFAULT: Duration = Duration::from_secs(900);

/// Per-client tuning.
#[derive(Clone, Copy, Debug)]
pub struct RegistryTuning {
    /// Send a liveness probe after this much protocol-frame silence.
    pub probe_quiet: Duration,
}

impl Default for RegistryTuning {
    fn default() -> Self {
        Self {
            probe_quiet: PROBE_QUIET_DEFAULT,
        }
    }
}

/// Connection/sync lifecycle notifications (best-effort broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryEvent {
    /// Joined (or re-joined); the hello state has been applied.
    Connected,
    /// The connection dropped; the client is backing off before redialing.
    Disconnected,
    /// Rows/acks were applied to the doc — republish and persist.
    Applied,
    /// A remote device's presence beat arrived.
    Presence,
}

// ── wire frames (JSON text; mirror edge/src/registry-room.ts) ───────────────

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ClientFrame<'a> {
    Hello {
        cursor: Option<u64>,
        device: &'a str,
    },
    Push {
        batch: &'a str,
        ops: &'a [zeron_doc::RowOp],
    },
    Presence {
        at: i64,
    },
    Probe,
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ServerFrame {
    State {
        seq: u64,
        full: bool,
        #[serde(rename = "gcFloor", default)]
        gc_floor: u64,
        rows: Vec<RegistryRow>,
        #[serde(default)]
        presence: HashMap<String, i64>,
    },
    Rows {
        seq: u64,
        rows: Vec<RegistryRow>,
    },
    Ack {
        batch: String,
        seq: u64,
        #[allow(dead_code)]
        applied: u64,
    },
    Presence {
        device: String,
        at: i64,
    },
    #[serde(rename = "probe-ok")]
    ProbeOk {
        #[allow(dead_code)]
        seq: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

// ── transport plumbing (text-frame sibling of room.rs's Pipe) ───────────────

pub(crate) struct TextPipe {
    pub(crate) tx: mpsc::Sender<String>,
    pub(crate) rx: mpsc::Receiver<String>,
}

pub(crate) trait TextConnector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<TextPipe, SyncError>>;
}

/// Plain-HTTPS pull/push seam — the airplane-wifi transport. `fetch` GETs
/// `/registry/{org}/rows?since=` (the WS hello's exact delta answer as JSON);
/// `push` POSTs one op batch to `/registry/{org}/push` and returns the ack
/// body. Both are safe at-least-once: LWW clocks make replays apply zero ops.
pub trait RegistryTransport: Send + Sync + 'static {
    fn fetch(&self, since: u64) -> BoxFuture<'static, Result<String, SyncError>>;
    fn push(&self, body: String) -> BoxFuture<'static, Result<String, SyncError>>;
}

struct WsTextConnector {
    url: Arc<dyn UrlProvider>,
}

impl TextConnector for WsTextConnector {
    fn connect(&self) -> BoxFuture<'static, Result<TextPipe, SyncError>> {
        let provider = self.url.clone();
        Box::pin(async move {
            let url = provider.url().await?;
            let ws = crate::dial::connect_ws(&url)
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            tokio::spawn(pump(ws, out_rx, in_tx));
            Ok(TextPipe {
                tx: out_tx,
                rx: in_rx,
            })
        })
    }
}

/// Shuttle text frames between the WebSocket and the actor's channels, plus
/// the ping keepalive and the transport silence lease.
async fn pump(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut out_rx: mpsc::Receiver<String>,
    in_tx: mpsc::Sender<String>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(text) => {
                    if sink.send(WsMessage::Text(text)).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    last_rx = tokio::time::Instant::now();
                    let text = text.to_string();
                    if text != "pong" && in_tx.send(text).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {
                    last_rx = tokio::time::Instant::now();
                }
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => {
                tracing::warn!("registry socket silent past lease; treating as dead");
                break;
            }
        }
    }
}

// ── stats (RoomStatsSnapshot-compatible so SyncStatus/`zeron sync` render it) ─

#[derive(Default)]
struct Stats {
    connected: std::sync::atomic::AtomicBool,
    /// Latched once a server state applies this process (hello or HTTPS
    /// pull) — never cleared by disconnects; see `RoomStatsSnapshot::synced`.
    synced: std::sync::atomic::AtomicBool,
    last_pushed_ms: std::sync::atomic::AtomicI64,
    last_ack_ms: std::sync::atomic::AtomicI64,
    rejoins: std::sync::atomic::AtomicU64,
    probes: std::sync::atomic::AtomicU64,
    full_resyncs: std::sync::atomic::AtomicU64,
    disconnects: std::sync::atomic::AtomicU64,
    rejected: std::sync::atomic::AtomicU64,
    /// Epoch ms of the next scheduled dial attempt (0 = connected or dialing
    /// right now). Drives the "Reconnecting — retrying in Ns" countdown.
    retry_at_ms: std::sync::atomic::AtomicI64,
    /// Monotonic dial-attempt counter; each attempt's number is its trace id
    /// in logs, so an incident reads as one numbered sequence.
    dial_seq: std::sync::atomic::AtomicU64,
    /// The failure that started/extended the current outage. Sticky: cleared
    /// only by a successful join, so the UI can keep the last failure visible
    /// through the next attempt instead of flickering to "connecting…".
    last_failure: Mutex<Option<String>>,
}

/// One room's reconnect posture, for connection-status UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconnectState {
    /// Epoch ms of the next scheduled dial (0 = none pending).
    pub retry_at_ms: i64,
    /// Failure that started the current outage; `None` once rejoined.
    pub last_failure: Option<String>,
}

impl Stats {
    fn snapshot(&self) -> RoomStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        RoomStatsSnapshot {
            connected: self.connected.load(Relaxed),
            synced: self.synced.load(Relaxed),
            last_pushed_ms: self.last_pushed_ms.load(Relaxed),
            last_ack_ms: self.last_ack_ms.load(Relaxed),
            rejoins: self.rejoins.load(Relaxed),
            probes: self.probes.load(Relaxed),
            full_resyncs: self.full_resyncs.load(Relaxed),
            disconnects: self.disconnects.load(Relaxed),
            rejected: self.rejected.load(Relaxed),
        }
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the client ──────────────────────────────────────────────────────────────

/// A live registry-room membership for one [`RegistryDoc`].
///
/// Owns a background actor that keeps the doc converged with the room. The
/// engine host mutates the doc under its own lock and calls [`Self::nudge`]
/// to push; server frames apply through the same lock.
pub struct RegistryClient {
    doc: Arc<Mutex<RegistryDoc>>,
    events: broadcast::Sender<RegistryEvent>,
    shutdown: watch::Sender<bool>,
    nudge: mpsc::Sender<()>,
    probe: mpsc::Sender<()>,
    redial: mpsc::Sender<()>,
    presence_out: mpsc::Sender<i64>,
    presence: Arc<Mutex<HashMap<String, (i64, tokio::time::Instant)>>>,
    stats: Arc<Stats>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RegistryClient {
    /// Connect (fixed URL — dev/tests).
    pub async fn connect(
        url: &str,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
    ) -> Result<Self, SyncError> {
        Self::connect_via(Arc::new(StaticUrl(url.to_string())), doc, device_id).await
    }

    /// Connect with a per-dial URL provider (fresh `?token=` every attempt).
    /// Resolves once the initial hello/state handshake lands; a first-attempt
    /// failure is returned as `Err` (callers own the initial-join retry, same
    /// contract as `ChatClient`). After that the client reconnects itself.
    pub async fn connect_via(
        provider: Arc<dyn UrlProvider>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
    ) -> Result<Self, SyncError> {
        Self::connect_via_tuned(provider, doc, device_id, RegistryTuning::default()).await
    }

    pub async fn connect_via_tuned(
        provider: Arc<dyn UrlProvider>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
        tuning: RegistryTuning,
    ) -> Result<Self, SyncError> {
        let connector = Arc::new(WsTextConnector { url: provider });
        Self::connect_with_transport(connector, doc, device_id, tuning, None).await
    }

    /// Connect with a plain-HTTPS pull/push seam alongside the socket (the
    /// airplane-wifi transport): an immediate delta GET bootstraps the doc
    /// in ~1 RTT while the WS dials, and every backoff cycle syncs over
    /// HTTPS so a network that never passes the upgrade still converges.
    pub async fn connect_via_transport(
        provider: Arc<dyn UrlProvider>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
        tuning: RegistryTuning,
        transport: Arc<dyn RegistryTransport>,
    ) -> Result<Self, SyncError> {
        let connector = Arc::new(WsTextConnector { url: provider });
        Self::connect_with_transport(connector, doc, device_id, tuning, Some(transport)).await
    }

    pub(crate) async fn connect_with_tuned(
        connector: Arc<dyn TextConnector>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
        tuning: RegistryTuning,
    ) -> Result<Self, SyncError> {
        Self::connect_with_transport(connector, doc, device_id, tuning, None).await
    }

    pub(crate) async fn connect_with_transport(
        connector: Arc<dyn TextConnector>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
        tuning: RegistryTuning,
        transport: Option<Arc<dyn RegistryTransport>>,
    ) -> Result<Self, SyncError> {
        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (nudge_tx, nudge_rx) = mpsc::channel(1);
        let (probe_tx, probe_rx) = mpsc::channel(1);
        let (redial_tx, redial_rx) = mpsc::channel(1);
        let (presence_tx, presence_rx) = mpsc::channel(4);
        let presence = Arc::new(Mutex::new(HashMap::new()));
        let stats = Arc::new(Stats::default());

        let actor = Actor {
            doc: doc.clone(),
            device_id: device_id.to_string(),
            connector,
            tuning,
            events: events.clone(),
            shutdown: shutdown_rx,
            nudge_rx,
            probe_rx,
            redial_rx,
            presence_rx,
            presence: presence.clone(),
            stats: stats.clone(),
            transport,
            sync_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let task = tokio::spawn(actor.run(ready_tx));

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                doc,
                events,
                shutdown: shutdown_tx,
                nudge: nudge_tx,
                probe: probe_tx,
                redial: redial_tx,
                presence_out: presence_tx,
                presence,
                stats,
                task: Some(task),
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

    pub fn doc(&self) -> &Arc<Mutex<RegistryDoc>> {
        &self.doc
    }

    pub fn events(&self) -> broadcast::Receiver<RegistryEvent> {
        self.events.subscribe()
    }

    /// Local writes were enqueued — push pending batches now.
    pub fn nudge(&self) {
        let _ = self.nudge.try_send(());
    }

    /// Publish this device's presence beat (epoch ms).
    pub fn set_presence(&self, at: i64) {
        let _ = self.presence_out.try_send(at);
    }

    /// Remote devices' live presence beats (entries within the 30s TTL),
    /// device → beat epoch ms.
    pub fn presence(&self) -> HashMap<String, i64> {
        let now = tokio::time::Instant::now();
        lock(&self.presence)
            .iter()
            .filter(|(_, (_, seen))| now.duration_since(*seen) < PRESENCE_TTL)
            .map(|(device, (at, _))| (device.clone(), *at))
            .collect()
    }

    /// Liveness hint: probe the room now (deadline-checked).
    pub fn probe(&self) {
        let _ = self.probe.try_send(());
    }

    /// Escalation: tear the session down and dial a fresh socket.
    pub fn redial(&self) {
        let _ = self.redial.try_send(());
    }

    pub fn stats(&self) -> RoomStatsSnapshot {
        self.stats.snapshot()
    }

    /// Current reconnect posture (next-dial deadline + sticky last failure)
    /// for connection-status UI.
    pub fn reconnect_state(&self) -> ReconnectState {
        ReconnectState {
            retry_at_ms: self
                .stats
                .retry_at_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            last_failure: lock(&self.stats.last_failure).clone(),
        }
    }

    /// Leave cleanly and stop the actor.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RegistryClient {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

// ── the actor ───────────────────────────────────────────────────────────────

struct Actor {
    doc: Arc<Mutex<RegistryDoc>>,
    device_id: String,
    connector: Arc<dyn TextConnector>,
    tuning: RegistryTuning,
    events: broadcast::Sender<RegistryEvent>,
    shutdown: watch::Receiver<bool>,
    nudge_rx: mpsc::Receiver<()>,
    probe_rx: mpsc::Receiver<()>,
    redial_rx: mpsc::Receiver<()>,
    presence_rx: mpsc::Receiver<i64>,
    presence: Arc<Mutex<HashMap<String, (i64, tokio::time::Instant)>>>,
    stats: Arc<Stats>,
    /// Plain-HTTPS pull/push (None = socket-only: tests, dev bearers).
    transport: Option<Arc<dyn RegistryTransport>>,
    /// One offline sync in flight at a time.
    sync_busy: Arc<std::sync::atomic::AtomicBool>,
}

enum SessionEnd {
    /// Transport died / probe deadline / requested redial: back off, redial.
    Reconnect,
    /// Shutdown requested: stop the actor.
    Stop,
}

/// How a backoff wait ended.
enum Waited {
    Elapsed,
    /// System wake or a sibling dial succeeded: redial NOW on fresh backoff.
    Woke,
    Shutdown,
}

impl Actor {
    async fn run(mut self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let mut ready = Some(ready);
        let mut backoff = BACKOFF_BASE;
        // Pull-first bootstrap: with an HTTPS transport, the doc syncs in
        // ~1 round trip while the socket spends its 4+ on TLS + upgrade +
        // hello — and construction resolves immediately (local-first: the
        // doc is usable now; keeping it converged is this actor's job, over
        // whichever transport the network permits). Socket-only clients
        // keep the original contract: ready resolves on the first join.
        if self.transport.is_some() {
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(()));
            }
            self.spawn_offline_sync();
        }
        // Suspend/resume and sibling-dial successes are EVENTS that end a
        // backoff wait immediately (see room.rs) — without them a recovered
        // network still waited out the full accumulated delay.
        let mut wake = crate::wake::subscribe();
        let mut online = crate::wake::subscribe_online();
        loop {
            if *self.shutdown.borrow() {
                return;
            }
            let attempt = self
                .stats
                .dial_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let dial = tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect()).await;
            let pipe = match dial {
                Ok(Ok(pipe)) => pipe,
                Ok(Err(err)) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(err));
                        return; // first join failed: caller owns the retry
                    }
                    tracing::warn!(error = %err, attempt, "registry dial failed; backing off");
                    self.note_failure(format!("dial failed: {err}"));
                    self.spawn_offline_sync();
                    match self.wait_backoff(&mut wake, &mut online, backoff).await {
                        Waited::Shutdown => return,
                        Waited::Woke => backoff = BACKOFF_BASE,
                        Waited::Elapsed => backoff = (backoff * 2).min(BACKOFF_CAP),
                    }
                    continue;
                }
                Err(_) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(SyncError::WebSocket("connect timeout".into())));
                        return;
                    }
                    tracing::warn!(attempt, "registry dial timed out; backing off");
                    self.note_failure("dial timed out".into());
                    self.spawn_offline_sync();
                    match self.wait_backoff(&mut wake, &mut online, backoff).await {
                        Waited::Shutdown => return,
                        Waited::Woke => backoff = BACKOFF_BASE,
                        Waited::Elapsed => backoff = (backoff * 2).min(BACKOFF_CAP),
                    }
                    continue;
                }
            };

            let session_started = tokio::time::Instant::now();
            match self.run_session(pipe, &mut ready).await {
                SessionEnd::Stop => return,
                SessionEnd::Reconnect => {
                    lock(&self.doc).mark_disconnected();
                    // Only a session that joined AND stayed healthy for a
                    // while earns a fresh backoff. Reset-on-join alone let a
                    // connect-and-die socket (captive portal, half-broken
                    // NAT) hot-loop at 250ms forever; without any reset, ~7
                    // flaps pinned every future reconnect at the cap for the
                    // life of the client.
                    let joined = self
                        .stats
                        .connected
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    self.stats
                        .disconnects
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.note_failure("connection dropped".into());
                    let _ = self.events.send(RegistryEvent::Disconnected);
                    if ready.is_some() {
                        // Handshake failed on the very first session.
                        if let Some(ready) = ready.take() {
                            let _ = ready
                                .send(Err(SyncError::Protocol("registry handshake failed".into())));
                        }
                        return;
                    }
                    if joined && session_started.elapsed() >= STABLE_RESET {
                        backoff = BACKOFF_BASE;
                    }
                    // Socket down: sync over HTTPS while we wait — pending
                    // pushes flush and the doc stays fresh at backoff cadence
                    // (≤16s), so a WS-hostile network degrades to polling
                    // instead of silence.
                    self.spawn_offline_sync();
                    match self.wait_backoff(&mut wake, &mut online, backoff).await {
                        Waited::Shutdown => return,
                        Waited::Woke => backoff = BACKOFF_BASE,
                        Waited::Elapsed => backoff = (backoff * 2).min(BACKOFF_CAP),
                    }
                }
            }
        }
    }

    /// Record the failure that started/extended the current outage (sticky
    /// until the next successful join — the UI keeps it visible through the
    /// next attempt instead of flickering back to a bare "connecting").
    fn note_failure(&self, msg: String) {
        *lock(&self.stats.last_failure) = Some(msg);
    }

    /// Sleep out one backoff, cut short by system wake, a sibling dial
    /// success, or shutdown. While the OS reports no network path, the wait
    /// parks on the event buses (with a coarse safety timer) instead of
    /// burning dial attempts that cannot succeed.
    async fn wait_backoff(
        &mut self,
        wake: &mut tokio::sync::broadcast::Receiver<()>,
        online: &mut tokio::sync::broadcast::Receiver<()>,
        wait: Duration,
    ) -> Waited {
        // Drain stale events: only wakes/successes DURING this wait count,
        // or our own last dial would cut every wait to zero.
        while wake.try_recv().is_ok() {}
        while online.try_recv().is_ok() {}
        use std::sync::atomic::Ordering::Relaxed;
        let wait = if crate::wake::path_is_offline() {
            wait.max(OFFLINE_PARK_RECHECK)
        } else {
            wait
        };
        self.stats
            .retry_at_ms
            .store(epoch_ms() + wait.as_millis() as i64, Relaxed);
        let waited = tokio::select! {
            _ = tokio::time::sleep(wait) => Waited::Elapsed,
            _ = wake.recv() => Waited::Woke,
            _ = online.recv() => Waited::Woke,
            _ = self.shutdown.changed() => {
                if *self.shutdown.borrow() {
                    Waited::Shutdown
                } else {
                    Waited::Elapsed
                }
            }
        };
        self.stats.retry_at_ms.store(0, Relaxed);
        waited
    }

    async fn run_session(
        &mut self,
        mut pipe: TextPipe,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> SessionEnd {
        use std::sync::atomic::Ordering::Relaxed;

        // ── hello / state handshake ─────────────────────────────────────────
        let cursor = {
            let doc = lock(&self.doc);
            let cursor = doc.cursor();
            if cursor == 0 && doc.pending_len() == 0 && doc.generation() == 0 {
                None
            } else {
                Some(cursor)
            }
        };
        let hello = serde_json::to_string(&ClientFrame::Hello {
            cursor,
            device: &self.device_id,
        })
        .expect("hello serializes");
        if pipe.tx.send(hello).await.is_err() {
            return SessionEnd::Reconnect;
        }
        let state = tokio::time::timeout(HELLO_DEADLINE, async {
            loop {
                match pipe.rx.recv().await {
                    Some(text) => match serde_json::from_str::<ServerFrame>(&text) {
                        Ok(frame @ ServerFrame::State { .. }) => return Some(frame),
                        Ok(_) => continue, // stale broadcast before our state
                        Err(err) => {
                            tracing::warn!(error = %err, "registry: bad frame during handshake");
                            return None;
                        }
                    },
                    None => None?,
                }
            }
        })
        .await;
        let Ok(Some(ServerFrame::State {
            seq,
            full,
            gc_floor,
            rows,
            presence,
        })) = state
        else {
            tracing::warn!("registry: no state frame within deadline");
            return SessionEnd::Reconnect;
        };
        {
            let mut doc = lock(&self.doc);
            let outcome = doc.apply_state(seq, full, gc_floor, rows);
            if full {
                self.stats.full_resyncs.fetch_add(1, Relaxed);
            }
            if outcome == StateOutcome::Reseeded {
                tracing::info!("registry: server behind local state; re-seeding");
            }
        }
        {
            let now = tokio::time::Instant::now();
            let mut map = lock(&self.presence);
            for (device, at) in presence {
                map.insert(device, (at, now));
            }
        }
        self.stats.connected.store(true, Relaxed);
        self.stats.synced.store(true, Relaxed);
        self.stats.last_pushed_ms.store(epoch_ms(), Relaxed);
        self.stats.retry_at_ms.store(0, Relaxed);
        *lock(&self.stats.last_failure) = None;
        if ready.is_none() {
            self.stats.rejoins.fetch_add(1, Relaxed);
        }
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let _ = self.events.send(RegistryEvent::Connected);
        let _ = self.events.send(RegistryEvent::Applied);

        // Anything pending (offline writes, reseeds, migration) pushes now.
        if !self.push_pending(&mut pipe).await {
            return SessionEnd::Reconnect;
        }

        // ── steady state ────────────────────────────────────────────────────
        let mut last_frame = tokio::time::Instant::now();
        let mut probe_deadline: Option<tokio::time::Instant> = None;
        loop {
            let quiet_probe_at = last_frame + self.tuning.probe_quiet;
            let deadline_at = probe_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
            tokio::select! {
                frame = pipe.rx.recv() => {
                    let Some(text) = frame else {
                        return SessionEnd::Reconnect;
                    };
                    last_frame = tokio::time::Instant::now();
                    probe_deadline = None;
                    if !self.handle_frame(&text) {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.nudge_rx.recv() => {
                    if !self.push_pending(&mut pipe).await {
                        return SessionEnd::Reconnect;
                    }
                }
                at = self.presence_rx.recv() => {
                    if let Some(at) = at {
                        let frame = serde_json::to_string(&ClientFrame::Presence { at })
                            .expect("presence serializes");
                        if pipe.tx.send(frame).await.is_err() {
                            return SessionEnd::Reconnect;
                        }
                    }
                }
                _ = self.probe_rx.recv() => {
                    if !self.send_probe(&mut pipe, &mut probe_deadline).await {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.redial_rx.recv() => {
                    tracing::info!("registry: redial requested");
                    return SessionEnd::Reconnect;
                }
                _ = tokio::time::sleep_until(quiet_probe_at) => {
                    if !self.send_probe(&mut pipe, &mut probe_deadline).await {
                        return SessionEnd::Reconnect;
                    }
                    // Don't re-arm the quiet timer against the same silence.
                    last_frame = tokio::time::Instant::now();
                }
                _ = tokio::time::sleep_until(deadline_at) => {
                    tracing::warn!("registry: probe unanswered past deadline; redialing");
                    return SessionEnd::Reconnect;
                }
                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
                        return SessionEnd::Stop;
                    }
                }
            }
        }
    }

    async fn send_probe(
        &self,
        pipe: &mut TextPipe,
        probe_deadline: &mut Option<tokio::time::Instant>,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.stats.probes.fetch_add(1, Relaxed);
        let frame = serde_json::to_string(&ClientFrame::Probe).expect("probe serializes");
        if pipe.tx.send(frame).await.is_err() {
            return false;
        }
        if probe_deadline.is_none() {
            *probe_deadline = Some(tokio::time::Instant::now() + PROBE_DEADLINE);
        }
        true
    }

    /// One HTTPS sync cycle off the actor's critical path: flush pending op
    /// batches (POST — LWW replays are no-ops), then pull the delta (GET) and
    /// apply it exactly as a state frame. Single-flight; failures are logged
    /// and retried on the next backoff cycle.
    fn spawn_offline_sync(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(transport) = self.transport.clone() else {
            return;
        };
        if self.sync_busy.swap(true, Relaxed) {
            return;
        }
        let doc = self.doc.clone();
        let events = self.events.clone();
        let presence = self.presence.clone();
        let stats = self.stats.clone();
        let busy = self.sync_busy.clone();
        tokio::spawn(async move {
            let batches: Vec<PendingBatch> = lock(&doc).take_pushable();
            let mut push_failed = false;
            for batch in &batches {
                let body =
                    serde_json::json!({ "batch": batch.batch, "ops": batch.ops }).to_string();
                match transport.push(body).await {
                    Ok(ack) => {
                        let parsed = serde_json::from_str::<serde_json::Value>(&ack)
                            .ok()
                            .and_then(|v| {
                                Some((v["batch"].as_str()?.to_owned(), v["seq"].as_u64()?))
                            });
                        match parsed {
                            Some((b, seq)) => {
                                lock(&doc).ack_batch(&b, seq);
                                stats.last_ack_ms.store(epoch_ms(), Relaxed);
                                let _ = events.send(RegistryEvent::Applied);
                            }
                            None => {
                                // An unreadable ack must count as a FAILED
                                // push: swallowing it left the batch marked
                                // in-flight, and on a pure-HTTPS device no
                                // socket disconnect ever comes along to
                                // un-mark it — the write was stranded until
                                // process restart.
                                tracing::warn!("registry: http push ack unreadable; will retry");
                                push_failed = true;
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "registry: http push failed; will retry");
                        push_failed = true;
                        break;
                    }
                }
            }
            if push_failed {
                // take_pushable marked them in flight; nothing will clear
                // that until the next socket disconnect — un-mark so the
                // next cycle (HTTP or WS) retries them.
                lock(&doc).mark_disconnected();
            }
            let since = lock(&doc).cursor();
            match transport.fetch(since).await {
                Ok(body) => {
                    #[derive(serde::Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct PullBody {
                        seq: u64,
                        full: bool,
                        gc_floor: u64,
                        rows: Vec<RegistryRow>,
                        #[serde(default)]
                        presence: HashMap<String, i64>,
                    }
                    match serde_json::from_str::<PullBody>(&body) {
                        Ok(pull) => {
                            let mut d = lock(&doc);
                            d.apply_state(pull.seq, pull.full, pull.gc_floor, pull.rows);
                            drop(d);
                            let now = tokio::time::Instant::now();
                            let mut map = lock(&presence);
                            for (device, at) in pull.presence {
                                map.insert(device, (at, now));
                            }
                            drop(map);
                            stats.synced.store(true, Relaxed);
                            stats.last_pushed_ms.store(epoch_ms(), Relaxed);
                            let _ = events.send(RegistryEvent::Applied);
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "registry: http pull body unparseable");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "registry: http pull failed; will retry");
                }
            }
            busy.store(false, Relaxed);
        });
    }

    async fn push_pending(&self, pipe: &mut TextPipe) -> bool {
        let batches: Vec<PendingBatch> = lock(&self.doc).take_pushable();
        for batch in batches {
            let frame = serde_json::to_string(&ClientFrame::Push {
                batch: &batch.batch,
                ops: &batch.ops,
            })
            .expect("push serializes");
            if pipe.tx.send(frame).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Apply one inbound protocol frame. False = protocol breakdown, redial.
    fn handle_frame(&self, text: &str) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let frame = match serde_json::from_str::<ServerFrame>(text) {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!(error = %err, "registry: unparseable frame");
                return false;
            }
        };
        match frame {
            ServerFrame::Rows { seq, rows } => {
                let contiguous = lock(&self.doc).apply_rows(seq, rows);
                self.stats.last_pushed_ms.store(epoch_ms(), Relaxed);
                let _ = self.events.send(RegistryEvent::Applied);
                if !contiguous {
                    // Frames between the cursor and this batch were missed;
                    // the cursor held. Reconnect — the fresh hello pulls the
                    // whole gap from the true cursor.
                    tracing::warn!(seq, "registry: broadcast seq gap; resyncing");
                    return false;
                }
            }
            ServerFrame::Ack { batch, seq, .. } => {
                lock(&self.doc).ack_batch(&batch, seq);
                self.stats.last_ack_ms.store(epoch_ms(), Relaxed);
                let _ = self.events.send(RegistryEvent::Applied);
            }
            ServerFrame::Presence { device, at } => {
                lock(&self.presence).insert(device, (at, tokio::time::Instant::now()));
                let _ = self.events.send(RegistryEvent::Presence);
            }
            ServerFrame::ProbeOk { .. } => {
                self.stats.last_pushed_ms.store(epoch_ms(), Relaxed);
            }
            ServerFrame::State {
                seq,
                full,
                gc_floor,
                rows,
                presence,
            } => {
                // Servers only send state as a hello answer, but applying a
                // late duplicate is harmless and simpler than special-casing.
                let mut doc = lock(&self.doc);
                doc.apply_state(seq, full, gc_floor, rows);
                drop(doc);
                let now = tokio::time::Instant::now();
                let mut map = lock(&self.presence);
                for (device, at) in presence {
                    map.insert(device, (at, now));
                }
                let _ = self.events.send(RegistryEvent::Applied);
            }
            ServerFrame::Error { code, message } => {
                self.stats.rejected.fetch_add(1, Relaxed);
                tracing::warn!(code, message, "registry: server rejected a frame");
            }
        }
        true
    }
}

#[cfg(any(test, feature = "mock-server"))]
pub mod mock_server;
