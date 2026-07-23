//! Device-room relay transport (ARCHITECTURE §1, feature-inventory §3.7): the byte-frame
//! codec spoken by the edge `DeviceRoom` DO, the **host relay** (this device serving its
//! full RPC surface through the relay), and the **client link** (dialing another device's
//! relay and speaking ordinary [`RpcClient`] RPC over it).
//!
//! Frame encoding (must stay byte-identical to `edge/src/device-room.ts`):
//! `uleb128(header_len) ‖ UTF-8 JSON header ‖ payload`, header `{s, k, to?, from?}`.
//! - client → DO: the DO stamps `from = connId` and forwards to the host socket;
//! - host → DO: must carry `to = connId`; the DO strips routing keys and delivers;
//! - relay control frames use kind [`RELAY_KIND`] with payload `{"error": code}` —
//!   codes `host_offline`, `host_closed`, `client_gone`, `client_closed`;
//! - nudge frames use kind [`NUDGE_KIND`] with payload `{"chatId": …}`.
//!
//! The RPC path multiplexes NOTHING new: each distinct client `connId` becomes a virtual
//! string-frame connection feeding the existing [`serve_connection`] seam, so every RPC
//! handler works through the relay untouched (the port of comet's `device-room-host.ts`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{RpcClient, RpcError, RpcService, serve_connection};

/// Relay-emitted control frames. MUST byte-match the DO's `RELAY_KIND` (yes, it has a
/// leading space — clients compare with equality; a mismatch makes host_offline invisible).
pub const RELAY_KIND: &str = " relay";
/// Durable command nudge frames (§7 cold-chat delivery): payload `{chatId}`.
pub const NUDGE_KIND: &str = "nudge";
/// The RPC stream over the relay: both `s` (stream id) and `k` (kind) are `"rpc"`.
pub const RPC_KIND: &str = "rpc";

/// Relay error codes (payload `{"error": code}` on [`RELAY_KIND`] frames).
pub const HOST_OFFLINE: &str = "host_offline";
pub const HOST_CLOSED: &str = "host_closed";
pub const CLIENT_GONE: &str = "client_gone";
pub const CLIENT_CLOSED: &str = "client_closed";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Text `"ping"` keepalive — answered by the DO's hibernation-safe auto-response
/// pair (`edge/src/device-room.ts`) without waking it.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Silence lease: every ping elicits an auto-pong, so a healthy socket sees
/// inbound traffic at least once per `PING_INTERVAL`. No inbound frame for a
/// full interval plus grace = dead socket (half-open TCP after NAT timeout or
/// sleep/wake) — drop it and reconnect instead of waiting on a TCP write error.
const SILENCE_LEASE: Duration = Duration::from_secs(45);

// ---------------------------------------------------------------------------
// Frame codec
// ---------------------------------------------------------------------------

/// The JSON frame header. Field order matters for byte-parity with the TS encoder
/// (`JSON.stringify` of `{s, k, to?, from?}`); absent routing keys are omitted, not null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFrameHeader {
    /// Stream id, unique per (connId, logical stream).
    pub s: String,
    /// Stream kind: `"rpc"` | `"term"` | … — opaque to the relay.
    pub k: String,
    /// Routing: host → client target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Routing: client → host origin (stamped by the relay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl DeviceFrameHeader {
    pub fn new(s: impl Into<String>, k: impl Into<String>) -> Self {
        Self {
            s: s.into(),
            k: k.into(),
            to: None,
            from: None,
        }
    }

    pub fn with_to(mut self, conn_id: impl Into<String>) -> Self {
        self.to = Some(conn_id.into());
        self
    }
}

/// Encode `uleb128(header_len) ‖ header JSON ‖ payload`.
pub fn encode_device_frame(
    header: &DeviceFrameHeader,
    payload: &[u8],
) -> Result<Vec<u8>, RpcError> {
    let json = serde_json::to_vec(header)
        .map_err(|e| RpcError::Transport(format!("encode frame header: {e}")))?;
    let mut out = Vec::with_capacity(json.len() + payload.len() + 5);
    let mut n = json.len();
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&json);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decode a device frame; the payload is the remainder after the JSON header.
pub fn decode_device_frame(bytes: &[u8]) -> Result<(DeviceFrameHeader, Vec<u8>), RpcError> {
    let bad = |m: &str| RpcError::Transport(format!("device frame: {m}"));
    let mut offset = 0usize;
    let mut len: usize = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(offset).ok_or_else(|| bad("truncated uleb128"))?;
        offset += 1;
        if shift >= 32 {
            return Err(bad("uleb128 overflow"));
        }
        len |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    let header_end = offset
        .checked_add(len)
        .ok_or_else(|| bad("header length overflow"))?;
    let header_bytes = bytes
        .get(offset..header_end)
        .ok_or_else(|| bad("truncated header"))?;
    let header: DeviceFrameHeader =
        serde_json::from_slice(header_bytes).map_err(|e| bad(&format!("bad header JSON: {e}")))?;
    Ok((header, bytes[header_end..].to_vec()))
}

/// Extract the error code from a relay control payload (`{"error": code}`).
pub fn relay_error_code(payload: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct RelayError {
        error: String,
    }
    serde_json::from_slice::<RelayError>(payload)
        .ok()
        .map(|e| e.error)
}

/// Build the device-room WebSocket URL from the http(s) edge base URL.
pub fn device_room_ws_url(
    edge_url: &str,
    device_id: &str,
    role: &str,
    conn_id: Option<&str>,
    token: &str,
) -> String {
    let ws_base = edge_url.replacen("http", "ws", 1);
    let ws_base = ws_base.trim_end_matches('/');
    let conn = conn_id.map(|c| format!("&connId={c}")).unwrap_or_default();
    format!("{ws_base}/device/{device_id}/ws?role={role}{conn}&token={token}")
}

// ---------------------------------------------------------------------------
// Token source — the auth seam
// ---------------------------------------------------------------------------

/// Fresh-bearer provider: the relay re-reads it on every (re)dial so an expired access
/// token is never reused after a refresh. `None` = signed out (host relay idles quietly).
#[async_trait]
pub trait TokenSource: Send + Sync + 'static {
    async fn token(&self) -> Option<String>;
}

/// A fixed token (dev mode / tests).
pub struct StaticToken(pub String);

#[async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Option<String> {
        Some(self.0.clone())
    }
}

// ---------------------------------------------------------------------------
// Host relay
// ---------------------------------------------------------------------------

/// Called with the chat id of every nudge frame ("this chat's doc has pending commands —
/// open it and drain"); the engine warms/opens the chat doc.
pub type NudgeHandler = Arc<dyn Fn(String) + Send + Sync>;

pub struct HostRelayConfig {
    /// Edge base URL (`http(s)://…`; rewritten to `ws(s)` for the socket).
    pub edge_url: String,
    pub device_id: String,
    pub token: Arc<dyn TokenSource>,
    /// Reconnect delay after a session ends (a small jitter is added).
    pub retry: Duration,
}

impl HostRelayConfig {
    pub fn new(
        edge_url: impl Into<String>,
        device_id: impl Into<String>,
        token: Arc<dyn TokenSource>,
    ) -> Self {
        Self {
            edge_url: edge_url.into(),
            device_id: device_id.into(),
            token,
            retry: Duration::from_secs(5),
        }
    }
}

/// The host end of the relay: one outbound WebSocket to our own DeviceRoom DO, serving
/// `service` to every client conn through virtual string-frame connections. Immortal
/// supervisor: quiet while signed out, reconnects with backoff when the socket drops
/// (including the 4409 "superseded by new host connection" close — the newest host wins,
/// so the superseded process backs off and retries, mirroring comet's DeviceRoomHost).
pub struct HostRelay {
    task: tokio::task::JoinHandle<()>,
}

impl HostRelay {
    pub fn spawn(
        config: HostRelayConfig,
        service: Arc<dyn RpcService>,
        on_nudge: NudgeHandler,
    ) -> Self {
        let task = tokio::spawn(async move {
            let mut wake = comet_sync::wake::subscribe();
            // Fast-rejoin bookkeeping: the edge DO periodically ends healthy
            // host sessions (hibernation/deploys). Every second the host is
            // away, client dials bounce with "readiness check failed" (user
            // report) — so a session that ENDED CLEANLY after a healthy
            // stretch rejoins near-instantly, and only rapid consecutive
            // failures walk the backoff.
            let mut delay = HOST_REJOIN_MIN;
            loop {
                if let Some(token) = config.token.token().await {
                    let url = device_room_ws_url(
                        &config.edge_url,
                        &config.device_id,
                        "host",
                        None,
                        &token,
                    );
                    let started = tokio::time::Instant::now();
                    let outcome = host_session(&url, &service, &on_nudge).await;
                    let healthy = started.elapsed() >= HOST_HEALTHY_SESSION;
                    match outcome {
                        Ok(()) => {
                            tracing::info!("device-room: host session ended; reconnecting");
                            delay = HOST_REJOIN_MIN;
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "device-room: host session failed");
                            delay = if healthy {
                                HOST_REJOIN_MIN
                            } else {
                                (delay * 2).min(config.retry)
                            };
                        }
                    }
                } else {
                    // Signed out: poll for credentials at the configured pace.
                    delay = config.retry;
                }
                tokio::select! {
                    _ = tokio::time::sleep(delay + jitter()) => {}
                    // Wake = redial NOW (the old socket died with the suspend).
                    _ = wake.recv() => { delay = HOST_REJOIN_MIN; }
                }
            }
        });
        Self { task }
    }
}

impl Drop for HostRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Floor for host-relay rejoins after a clean/healthy session end — the DO
/// evicting an idle host must not leave a multi-second unreachable window.
const HOST_REJOIN_MIN: Duration = Duration::from_millis(250);
/// In-call dial retries (see [`LinkCache::client`]): total attempts, and the
/// spacing multiplier between them (1.5s, then 3s — ~4.5s of cover, several
/// times the host's post-eviction rejoin window).
const DIAL_ATTEMPTS: u32 = 3;
const DIAL_RETRY_SPACING: Duration = Duration::from_millis(1500);
/// A session that lasted at least this long counts as healthy: its end is the
/// DO's lifecycle, not our failure, so the rejoin does not escalate.
const HOST_HEALTHY_SESSION: Duration = Duration::from_secs(30);

fn jitter() -> Duration {
    // Cheap decorrelation without a rand dependency.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis(u64::from(nanos) % 2_000)
}

/// One per-client virtual connection: `in_tx` feeds the ndjson dispatch loop
/// ([`serve_connection`]); its replies are pumped back as `{to: connId}` frames.
///
/// Teardown is by channel closure, NOT task abort: dropping `in_tx` ends the dispatch
/// loop, which aborts its in-flight request tasks (streams included); their reply senders
/// drop and the pump task drains out. Aborting the dispatch loop directly would strand
/// the request tasks it spawned.
struct VirtualConn {
    in_tx: mpsc::Sender<String>,
}

fn make_virtual_conn(
    service: Arc<dyn RpcService>,
    conn_id: String,
    host_out: mpsc::Sender<Vec<u8>>,
) -> VirtualConn {
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let (srv_out_tx, mut srv_out_rx) = mpsc::channel::<String>(256);
    tokio::spawn(serve_connection(service, srv_out_tx, in_rx));
    tokio::spawn(async move {
        while let Some(text) = srv_out_rx.recv().await {
            let header = DeviceFrameHeader::new(RPC_KIND, RPC_KIND).with_to(conn_id.clone());
            match encode_device_frame(&header, text.as_bytes()) {
                Ok(frame) => {
                    if host_out.send(frame).await.is_err() {
                        break; // relay socket gone
                    }
                }
                Err(err) => tracing::error!(error = %err, "device-room: frame encode failed"),
            }
        }
    });
    VirtualConn { in_tx }
}

/// One relay session: connect as host, serve RPC per client conn, until the socket drops.
async fn host_session(
    url: &str,
    service: &Arc<dyn RpcService>,
    on_nudge: &NudgeHandler,
) -> Result<(), RpcError> {
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| RpcError::Transport(format!("device room unreachable: {e}")))?;
    tracing::info!("device-room: host connected");
    let (mut sink, mut stream) = ws.split();
    // All writers (per-conn pumps) funnel through one outbound queue → one socket writer.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let mut conns: HashMap<String, VirtualConn> = HashMap::new();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // consume the immediate first tick
    let mut last_rx = tokio::time::Instant::now();

    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(bytes) => {
                    if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => break, // unreachable: we hold out_tx
            },
            message = stream.next() => match message {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    last_rx = tokio::time::Instant::now();
                    handle_host_frame(&bytes, &mut conns, service, &out_tx, on_nudge).await;
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    if let Some(frame) = frame {
                        tracing::info!(code = %frame.code, reason = %frame.reason,
                            "device-room: host socket closed by relay");
                    }
                    break;
                }
                Some(Err(_)) | None => break,
                // Text "pong" / control frames: proof of life for the lease.
                Some(Ok(_)) => last_rx = tokio::time::Instant::now(),
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => {
                tracing::warn!("device-room: host socket silent past lease; reconnecting");
                break;
            }
        }
    }
    // Dropping the conns aborts every per-client dispatch loop (terminals etc. reaped).
    conns.clear();
    Ok(())
}

async fn handle_host_frame(
    bytes: &[u8],
    conns: &mut HashMap<String, VirtualConn>,
    service: &Arc<dyn RpcService>,
    out_tx: &mpsc::Sender<Vec<u8>>,
    on_nudge: &NudgeHandler,
) {
    let (header, payload) = match decode_device_frame(bytes) {
        Ok(frame) => frame,
        Err(err) => {
            tracing::warn!(error = %err, "device-room: malformed frame — skipping");
            return;
        }
    };
    if header.k == RELAY_KIND {
        // Relay control: a client went away (`client_closed` carries `from`; a bounced
        // `client_gone` carries `to`) — tear down that conn's RPC server.
        let code = relay_error_code(&payload).unwrap_or_default();
        if let Some(conn_id) = header.from.as_deref().or(header.to.as_deref()) {
            tracing::debug!(conn = %conn_id, %code, "device-room: client conn torn down");
            conns.remove(conn_id);
        }
        return;
    }
    if header.k == NUDGE_KIND {
        // Durable command nudge (§7): open the chat doc so drain fires.
        #[derive(Deserialize)]
        struct Nudge {
            #[serde(rename = "chatId")]
            chat_id: Option<String>,
        }
        match serde_json::from_slice::<Nudge>(&payload) {
            Ok(Nudge {
                chat_id: Some(chat_id),
            }) => on_nudge(chat_id),
            _ => tracing::warn!("device-room: malformed nudge — ignoring"),
        }
        return;
    }
    if header.k != RPC_KIND {
        return; // future stream kinds (term, tunnel)
    }
    let Some(from) = header.from else {
        return;
    };
    let conn = conns
        .entry(from.clone())
        .or_insert_with(|| make_virtual_conn(service.clone(), from, out_tx.clone()));
    let text = String::from_utf8_lossy(&payload).into_owned();
    if conn.in_tx.send(text).await.is_err() {
        tracing::warn!("device-room: virtual conn dispatch loop gone");
    }
}

// ---------------------------------------------------------------------------
// Client link
// ---------------------------------------------------------------------------

/// The client end: one WebSocket to a peer device's relay carrying a single RPC stream,
/// exposed as an ordinary [`RpcClient`]. `host_offline` / `host_closed` relay frames (and
/// socket drops) mark the link down — in-flight calls fail with [`RpcError::Closed`] and
/// [`LinkCache`] evicts the entry so the next call re-dials.
pub struct DeviceLink {
    client: Arc<RpcClient>,
    closed_rx: watch::Receiver<Option<String>>,
    pump: tokio::task::JoinHandle<()>,
}

impl DeviceLink {
    pub async fn connect(url: &str) -> Result<Self, RpcError> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| RpcError::Transport(format!("device room unreachable: {e}")))?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
        let (in_tx, in_rx) = mpsc::channel::<String>(256);
        let (closed_tx, closed_rx) = watch::channel::<Option<String>>(None);

        let pump = tokio::spawn(async move {
            let mut ping = tokio::time::interval(PING_INTERVAL);
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ping.tick().await; // consume the immediate first tick
            let mut last_rx = tokio::time::Instant::now();
            let reason = loop {
                tokio::select! {
                    frame = out_rx.recv() => match frame {
                        Some(text) => {
                            let header = DeviceFrameHeader::new(RPC_KIND, RPC_KIND);
                            let encoded = match encode_device_frame(&header, text.as_bytes()) {
                                Ok(bytes) => bytes,
                                Err(err) => {
                                    tracing::error!(error = %err, "device-room: frame encode failed");
                                    continue;
                                }
                            };
                            if sink.send(WsMessage::Binary(encoded)).await.is_err() {
                                break "connection lost".to_string();
                            }
                        }
                        None => {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            break "closed".to_string();
                        }
                    },
                    message = stream.next() => match message {
                        Some(Ok(WsMessage::Binary(bytes))) => {
                            last_rx = tokio::time::Instant::now();
                            match decode_device_frame(&bytes) {
                                Ok((header, payload)) if header.k == RELAY_KIND => {
                                    // host_offline / host_closed: surface as link-down.
                                    let code = relay_error_code(&payload)
                                        .unwrap_or_else(|| "relay error".into());
                                    tracing::info!(%code, "device-room: link down");
                                    break code;
                                }
                                Ok((header, payload)) if header.k == RPC_KIND => {
                                    let text = String::from_utf8_lossy(&payload).into_owned();
                                    if in_tx.send(text).await.is_err() {
                                        break "client dropped".to_string();
                                    }
                                }
                                Ok(_) => {}
                                Err(err) => {
                                    tracing::warn!(error = %err, "device-room: malformed frame");
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => {
                            break "connection lost".to_string();
                        }
                        // Text "pong" / control frames: proof of life for the lease.
                        Some(Ok(_)) => last_rx = tokio::time::Instant::now(),
                    },
                    _ = ping.tick() => {
                        if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                            break "connection lost".to_string();
                        }
                    }
                    _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => {
                        break "silent past lease".to_string();
                    }
                }
            };
            // Dropping in_tx ends the RpcClient reader → pending calls fail Closed.
            let _ = closed_tx.send(Some(reason));
        });

        Ok(Self {
            client: Arc::new(RpcClient::new(out_tx, in_rx)),
            closed_rx,
            pump,
        })
    }

    pub fn client(&self) -> Arc<RpcClient> {
        self.client.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.closed_rx.borrow().is_some()
    }

    /// Watch that resolves to `Some(reason)` when the link drops.
    pub fn closed(&self) -> watch::Receiver<Option<String>> {
        self.closed_rx.clone()
    }
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

// ---------------------------------------------------------------------------
// Link cache
// ---------------------------------------------------------------------------

pub struct LinkCacheConfig {
    pub edge_url: String,
    pub token: Arc<dyn TokenSource>,
    /// Exponential dial cooldown after failures (base, cap) — a dead peer must not be
    /// redialed at full cadence; callers fail fast in between (comet peers.ts behavior).
    pub cooldown_base: Duration,
    pub cooldown_max: Duration,
    /// Readiness probe budget: the relay accepts client joins even when the host is
    /// offline, so a `ListHarnesses` round-trip proves the path before caching.
    pub probe_timeout: Duration,
}

impl LinkCacheConfig {
    pub fn new(edge_url: impl Into<String>, token: Arc<dyn TokenSource>) -> Self {
        // Interactive remote control (remote folders, terminals, accounts) rides
        // this cache: one blip must cost seconds, not minutes. The old comet
        // 15s→5min curve punished a single failed dial with a 5-minute refusal;
        // here the first failure backs off 5s and even a dead peer is re-probed
        // within a minute. A generous probe budget keeps a slow-waking laptop
        // (radio up, engine still thawing) from counting as a failure.
        Self {
            edge_url: edge_url.into(),
            token,
            cooldown_base: Duration::from_secs(5),
            cooldown_max: Duration::from_secs(60),
            probe_timeout: Duration::from_secs(10),
        }
    }
}

/// Consecutive failures older than this decay to zero — a blip an hour ago must
/// not escalate today's first retry up the backoff curve.
const FAILURE_DECAY: Duration = Duration::from_secs(600);

#[derive(Default)]
struct DialState {
    failures: u32,
    last_failure: Option<Instant>,
    cooldown_until: Option<Instant>,
}

/// Lazily-dialed, cached peer links keyed by device id — the Rust twin of comet's
/// `Peers`. Cache hits never wait behind an in-flight dial; dials to the same device are
/// serialized per device (a global lock would head-of-line-block healthy peers); links
/// self-evict when the transport drops; a failed RPC should call [`LinkCache::invalidate`]
/// so the next call re-dials.
pub struct LinkCache {
    config: LinkCacheConfig,
    links: Mutex<HashMap<String, Arc<DeviceLink>>>,
    dial_state: Mutex<HashMap<String, DialState>>,
    dial_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl LinkCache {
    pub fn new(config: LinkCacheConfig) -> Arc<Self> {
        let cache = Arc::new(Self {
            config,
            links: Mutex::new(HashMap::new()),
            dial_state: Mutex::new(HashMap::new()),
            dial_locks: Mutex::new(HashMap::new()),
        });
        // Wake = every cached link is half-open and every cooldown is moot
        // (the failures belonged to the pre-suspend network). Drop them so the
        // next call redials immediately with fresh credentials. Skipped
        // outside a runtime (sync unit tests).
        if tokio::runtime::Handle::try_current().is_ok() {
            let weak = Arc::downgrade(&cache);
            tokio::spawn(async move {
                let mut wake = comet_sync::wake::subscribe();
                while wake.recv().await.is_ok() {
                    let Some(cache) = weak.upgrade() else { return };
                    lock(&cache.links).clear();
                    lock(&cache.dial_state).clear();
                    tracing::info!("peer: links + cooldowns cleared after wake");
                }
            });
        }
        cache
    }

    /// A live `RpcClient` to `device_id`'s engine (dialed + cached on first use).
    /// Transient dial failures retry in place a couple of times before
    /// surfacing: the host relay's DO periodically ends its session and
    /// rejoins within ~a second, and a user-facing call landing in that
    /// window should ride over it, not error (user report: refs/folders
    /// "unstable" vs the old app).
    pub async fn client(self: &Arc<Self>, device_id: &str) -> Result<Arc<RpcClient>, RpcError> {
        // Fast path outside any lock.
        if let Some(link) = self.cached(device_id) {
            return Ok(link.client());
        }
        let dial_lock = {
            let mut locks = lock(&self.dial_locks);
            locks.entry(device_id.to_string()).or_default().clone()
        };
        let _guard = dial_lock.lock().await;
        // Re-check under the per-device lock: a concurrent dial may have won.
        if let Some(link) = self.cached(device_id) {
            return Ok(link.client());
        }
        if let Some(message) = self.cooling(device_id) {
            return Err(RpcError::Transport(message));
        }
        let mut last_err = None;
        for attempt in 0..DIAL_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(DIAL_RETRY_SPACING * attempt).await;
            }
            match self.dial(device_id).await {
                Ok(link) => {
                    lock(&self.dial_state).remove(device_id);
                    lock(&self.links).insert(device_id.to_string(), link.clone());
                    self.spawn_evictor(device_id.to_string(), &link);
                    tracing::info!(device = %device_id, "peer: connected via device room");
                    return Ok(link.client());
                }
                Err(err) => {
                    tracing::debug!(device = %device_id, attempt, error = %err, "peer: dial attempt failed");
                    last_err = Some(err);
                }
            }
        }
        // Only the exhausted sequence counts as ONE failure on the cooldown
        // curve — the in-call retries must not escalate it by themselves.
        self.note_failure(device_id);
        Err(last_err.unwrap_or(RpcError::Closed))
    }

    /// Drop a cached link after a failed RPC so the next call re-dials.
    pub fn invalidate(&self, device_id: &str) {
        lock(&self.links).remove(device_id);
    }

    /// Data-driven cooldown reset: called when out-of-band evidence says the
    /// peer is alive again (fresh workspace presence heartbeat). The next call
    /// dials immediately instead of waiting out the backoff window.
    pub fn reset_cooldown(&self, device_id: &str) {
        if lock(&self.dial_state).remove(device_id).is_some() {
            tracing::info!(device = %device_id, "peer: cooldown cleared (peer is alive)");
        }
    }

    fn cached(&self, device_id: &str) -> Option<Arc<DeviceLink>> {
        let mut links = lock(&self.links);
        match links.get(device_id) {
            Some(link) if !link.is_closed() => Some(link.clone()),
            Some(_) => {
                links.remove(device_id);
                None
            }
            None => None,
        }
    }

    fn cooling(&self, device_id: &str) -> Option<String> {
        let state = lock(&self.dial_state);
        let entry = state.get(device_id)?;
        let until = entry.cooldown_until?;
        let now = Instant::now();
        if now >= until {
            return None;
        }
        Some(format!(
            "peer {device_id}: unreachable (backing off after {} failed dials; retrying in ~{}s)",
            entry.failures,
            (until - now).as_secs().max(1)
        ))
    }

    fn note_failure(&self, device_id: &str) {
        let mut state = lock(&self.dial_state);
        let entry = state.entry(device_id.to_string()).or_default();
        // Stale streaks restart the curve rather than escalating it.
        if entry
            .last_failure
            .is_some_and(|at| at.elapsed() > FAILURE_DECAY)
        {
            entry.failures = 0;
        }
        entry.last_failure = Some(Instant::now());
        entry.failures += 1;
        let backoff = self
            .config
            .cooldown_base
            .saturating_mul(1u32 << (entry.failures - 1).min(16))
            .min(self.config.cooldown_max);
        entry.cooldown_until = Some(Instant::now() + backoff);
    }

    async fn dial(&self, device_id: &str) -> Result<Arc<DeviceLink>, RpcError> {
        // Fresh token on every attempt — an expired one is never reused.
        let token = self
            .config
            .token
            .token()
            .await
            .ok_or_else(|| RpcError::Transport("not signed in".into()))?;
        let conn_id = uuid::Uuid::new_v4().to_string();
        let url = device_room_ws_url(
            &self.config.edge_url,
            device_id,
            "client",
            Some(&conn_id),
            &token,
        );
        tracing::info!(device = %device_id, "peer: dialing via device room");
        let link = Arc::new(DeviceLink::connect(&url).await?);
        // Readiness probe: prove the host answers before caching (an offline host bounces
        // host_offline, which closes the link and fails this call fast).
        let client = link.client();
        let probe = client.call(crate::methods::LIST_HARNESSES, serde_json::json!({}));
        tokio::time::timeout(self.config.probe_timeout, probe)
            .await
            .map_err(|_| {
                RpcError::Transport(format!("peer {device_id}: readiness check timed out"))
            })?
            .map_err(|e| {
                RpcError::Transport(format!("peer {device_id}: readiness check failed: {e}"))
            })?;
        Ok(link)
    }

    fn spawn_evictor(self: &Arc<Self>, device_id: String, link: &Arc<DeviceLink>) {
        let mut closed = link.closed();
        let cache = Arc::downgrade(self);
        let link_ptr = Arc::as_ptr(link) as usize;
        tokio::spawn(async move {
            loop {
                if closed.borrow().is_some() {
                    break;
                }
                if closed.changed().await.is_err() {
                    break;
                }
            }
            let Some(cache) = cache.upgrade() else { return };
            let mut links = lock(&cache.links);
            // Evict only if this exact link is still the cached one.
            if links
                .get(&device_id)
                .is_some_and(|l| Arc::as_ptr(l) as usize == link_ptr)
            {
                tracing::info!(device = %device_id, "peer: link dropped — evicting");
                links.remove(&device_id);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Codec tests — vectors ported from edge/src/device-frame.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn header(s: &str, k: &str) -> DeviceFrameHeader {
        DeviceFrameHeader::new(s, k)
    }

    #[test]
    fn round_trips_header_and_payload() {
        // device-frame.test.ts: "round-trips header + payload"
        let payload = [1u8, 2, 3, 250, 255];
        let h = header("term-42", "term").with_to("conn-9");
        let frame = encode_device_frame(&h, &payload).expect("encode");
        let (decoded, out) = decode_device_frame(&frame).expect("decode");
        assert_eq!(decoded, h);
        assert_eq!(out, payload);
    }

    #[test]
    fn handles_empty_payloads_and_long_headers() {
        // device-frame.test.ts: "handles empty payloads and long headers" — the 200-char
        // stream id forces a multi-byte uleb128 length prefix.
        let mut h = header(&"x".repeat(200), "rpc");
        h.from = Some("conn-1".into());
        let frame = encode_device_frame(&h, &[]).expect("encode");
        let json_len = serde_json::to_vec(&h).expect("json").len();
        assert!(json_len > 0x7f, "vector must exercise multi-byte uleb128");
        assert_eq!(frame[0], (json_len & 0x7f) as u8 | 0x80);
        assert_eq!(frame[1], (json_len >> 7) as u8);
        let (decoded, out) = decode_device_frame(&frame).expect("decode");
        assert_eq!(decoded, h);
        assert!(out.is_empty());
    }

    #[test]
    fn byte_parity_with_ts_encoder() {
        // Byte-exact fixture computed from the TS encoder (uleb128 ‖ JSON.stringify
        // key order s,k,to,from ‖ payload).
        let frame = encode_device_frame(&header("a", "rpc"), &[1, 2]).expect("encode");
        let expected_json = br#"{"s":"a","k":"rpc"}"#;
        assert_eq!(frame[0] as usize, expected_json.len());
        assert_eq!(&frame[1..1 + expected_json.len()], expected_json);
        assert_eq!(&frame[1 + expected_json.len()..], &[1, 2]);

        let routed =
            encode_device_frame(&header("s1", "term").with_to("c9"), b"x").expect("encode");
        let expected = br#"{"s":"s1","k":"term","to":"c9"}"#;
        assert_eq!(routed[0] as usize, expected.len());
        assert_eq!(&routed[1..1 + expected.len()], expected);
    }

    #[test]
    fn decodes_relay_control_payloads() {
        let payload = br#"{"error":"host_offline"}"#;
        assert_eq!(relay_error_code(payload).as_deref(), Some(HOST_OFFLINE));
        assert_eq!(relay_error_code(b"not json"), None);
    }

    #[test]
    fn rejects_malformed_frames() {
        assert!(decode_device_frame(&[]).is_err()); // empty: truncated uleb128
        assert!(decode_device_frame(&[0x85]).is_err()); // continuation bit, no next byte
        assert!(decode_device_frame(&[10, b'{']).is_err()); // truncated header
        let mut minimal = vec![15u8];
        minimal.extend_from_slice(br#"{"s":"a","k":"b"}"#[..15].as_ref()); // wrong len: truncated JSON
        assert!(decode_device_frame(&minimal).is_err());
        let mut valid = vec![17u8];
        valid.extend_from_slice(br#"{"s":"a","k":"b"}"#);
        valid.push(9); // trailing payload byte
        let (h, p) = decode_device_frame(&valid).expect("valid minimal frame");
        assert_eq!((h.s.as_str(), h.k.as_str()), ("a", "b"));
        assert_eq!(p, vec![9]);
        assert!(decode_device_frame(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).is_err()); // overflow
    }

    #[test]
    fn ws_url_shapes() {
        let url = device_room_ws_url(
            "https://edge.example/",
            "dev-1",
            "client",
            Some("c1"),
            "tok",
        );
        assert_eq!(
            url,
            "wss://edge.example/device/dev-1/ws?role=client&connId=c1&token=tok"
        );
        let host = device_room_ws_url("http://localhost:26640", "d", "host", None, "t");
        assert_eq!(host, "ws://localhost:26640/device/d/ws?role=host&token=t");
    }
}
