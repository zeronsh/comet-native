//! Shared client-side sync types: the error surface, the per-dial URL/token
//! seam, and the stats snapshot behind `SyncStatus` / `zeron sync`.
//!
//! These lived in the legacy s2 room client (`room.rs`) until the chat2
//! cutover retired it; the registry and chat2 clients keep speaking the same
//! vocabulary.

use futures::future::BoxFuture;

/// Errors surfaced by the sync clients.
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
    #[error("auth: {0}")]
    Auth(String),
    #[error("client is shut down")]
    Closed,
}

/// Per-dial WebSocket URL provider — consulted before EVERY connection attempt,
/// including background reconnects, so a short-lived auth token embedded in the
/// URL (`?token=…`) is re-read fresh rather than frozen at first connect.
/// Return [`SyncError::Auth`] when no valid credential is available (signed
/// out); the reconnect loop backs off and retries.
pub trait UrlProvider: Send + Sync + 'static {
    fn url(&self) -> BoxFuture<'static, Result<String, SyncError>>;
}

/// Fixed URL (dev bearers and tests — tokens that never expire).
pub struct StaticUrl(pub String);

impl UrlProvider for StaticUrl {
    fn url(&self) -> BoxFuture<'static, Result<String, SyncError>> {
        let url = self.0.clone();
        Box::pin(async move { Ok(url) })
    }
}

/// Live sync introspection for one room — the data behind the engine's
/// `SyncStatus` RPC and `zeron sync`. Every 2026-08 incident was debugged
/// blind because none of this was observable at runtime.
#[derive(Debug, Clone, Default)]
pub struct RoomStatsSnapshot {
    /// A join is currently established.
    pub connected: bool,
    /// A server state has been APPLIED this process (WS hello answer or
    /// HTTPS pull) — the local replica has heard authoritative truth at
    /// least once this boot. Destructive repair (the orphan sweep) gates on
    /// this: before it, "row absent locally" is not evidence of deletion.
    pub synced: bool,
    /// Epoch ms of the last SERVER-PUSHED frame (broadcast, backfill,
    /// join answer) — 0 = never. The deaf-socket tell: fresh acks + stale
    /// pushes.
    pub last_pushed_ms: i64,
    /// Epoch ms of the last ack for our own writes — 0 = never.
    pub last_ack_ms: i64,
    /// Mid-session rejoins (reconnect resyncs, stale-peer, full resyncs).
    pub rejoins: u64,
    /// Liveness probes sent (background cadence + on-demand hints).
    pub probes: u64,
    /// Full-snapshot resyncs requested after failed imports.
    pub full_resyncs: u64,
    /// Sessions lost (transport drops, deadlines, requested redials).
    pub disconnects: u64,
    /// Our writes the server REJECTED (InvalidUpdate/PermissionDenied acks).
    /// Nonzero while `last_ack_ms` goes stale is the latched-session tell.
    pub rejected: u64,
}
