//! WorkspaceHost — owns the per-user workspace **registry** (docs/
//! registry-sync.md; replaces the Loro workspace doc after the 2026-07/08
//! wedge incidents): local snapshot persistence, edge room sync
//! (`/registry/{orgId}/ws` → room `reg1/{orgId}/{userId}`, offline-tolerant —
//! spaces/sessions are private to their owner, never org-visible), the device
//! registry row for THIS device, and the typed watch channels the
//! WatchChats/WatchDevices/WatchSessions RPC streams are fed from.
//!
//! Writer discipline (kept from the doc schema): this host writes its own device row,
//! its own session-status rows, and rows for chats it hosts; renames/archives are LWW
//! sets accepted from any device (the Mutate surface).
//!
//! Liveness: `lastSeenAt` is a row write on boot/shutdown ONLY — the periodic 15s
//! heartbeat rides the room's presence frames (memory-only on the DO), so staying
//! online never grows server state.
//!
//! Migration: first boot after the update finds no `registry1` snapshot, reads
//! the legacy `workspace2` Loro snapshot, and seeds the registry from it as
//! pending upserts (historical HLCs — live writes always win). The overlay
//! serves the full sidebar before any server contact; the old `ws4` rooms are
//! simply never joined again. The legacy snapshot is kept for rollback.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use chrono::Utc;
use tokio::sync::watch;

use zeron_doc::{DeletedSpace, REGISTRY_DOC_ID, RegistryDoc, WorkspaceDoc};
use zeron_proto::{Chat, ChatConfig, Device, Session, Space};
use zeron_sync::{DocsStore, RegistryClient, RegistryTuning};

use crate::doc_host::EdgeConfig;
use crate::{EngineError, now_ms};

/// Legacy Loro workspace snapshot row — now only read once, as the migration
/// source for the registry seed. Kept on disk for rollback.
pub const WORKSPACE_DOC_ID: &str = "workspace2";
/// Legacy (pre-spaces) snapshot row — best-effort deleted on open.
const LEGACY_WORKSPACE_DOC_ID: &str = "workspace";
/// Org used when none is configured (matches the edge's dev-mode `user@org` bearers).
pub const DEFAULT_ORG_ID: &str = "dev-org";
/// User used when none is configured (dev mode without a bearer).
pub const DEFAULT_USER_ID: &str = "dev-user";
/// Presence beat cadence.
const PRESENCE_INTERVAL_MS: u64 = 15_000;
/// Dial-gate thresholds (`peer_liveness`): a device is judged `Dark` — dials
/// parked with zero network traffic — only on POSITIVE evidence: our registry
/// room joined and warm past the warm-up window, and the device's freshest
/// known heartbeat (live map ∪ session cache ∪ durable row) older than the
/// dark window. m1-laptop shape: offline 6 days, yet hot-dialed in 3-dial
/// bursts every ~60s for the life of the app.
const DIAL_GATE_WARMUP_MS: i64 = 60_000;
const DIAL_GATE_DARK_MS: i64 = 5 * 60_000;

/// A presence heartbeat younger than this marks the device alive (3 missed
/// beats = offline). Also the "peer is reachable" signal that clears the
/// peer-dial cooldown.
const PRESENCE_FRESH_MS: i64 = 45_000;
/// Relay-status probe cadence. Presence heartbeats ride the registry room, so
/// any registry pathology (or our own room connection being down) silently
/// starves them — and every device looks offline while its relay works fine.
/// Before believing "offline", ask the device's DeviceRoom
/// (`GET /device/{id}/status` → `hostConnected`), which tracks the host socket
/// authoritatively and shares no machinery with the registry room. Probes only
/// run for devices whose heartbeat is stale, so the steady state (healthy
/// room, fresh beats) sends no extra traffic.
const RELAY_PROBE_INTERVAL_MS: u64 = 30_000;
/// Per-request timeout for a relay-status probe.
const RELAY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Debounce window for local snapshot saves after a change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;
/// Initial-join retry backoff (base, cap). A first registry-room join that
/// fails must not strand the device offline until an app restart — retry until
/// it lands. Jittered so N devices restarting together don't resynchronize
/// their retries into a thundering herd on the cold DO.
pub(crate) const JOIN_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);
/// 16s, matching the room clients' BACKOFF_CAP: the cap only bounds the dark
/// window when every event wake (online bus, system wake, token change)
/// missed, so it trades a few extra dials/minute for a tighter worst case.
pub(crate) const JOIN_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(16);

pub(crate) async fn token_changed(changes: &mut Option<tokio::sync::watch::Receiver<u64>>) {
    match changes {
        Some(changes) => {
            let _ = changes.changed().await;
        }
        None => std::future::pending::<()>().await,
    }
}

async fn token_revoked(token: &Option<Arc<dyn zeron_rpc::TokenSource>>) -> bool {
    match token {
        Some(token) => token.token().await.is_none(),
        // Fixed test/dev URLs have no revocable credential source.
        None => false,
    }
}

/// Quiet-probe cadence for the registry room: fixed at 15 minutes. One room
/// per engine, so the fixed cadence costs ~100 DO wakes/day total, and the
/// probe is deadline-checked — a mute room is detected within
/// probe cadence + 10s instead of hours (2026-08-04 deaf-socket lesson).
const REGISTRY_PROBE_QUIET: std::time::Duration = std::time::Duration::from_secs(900);
/// Deaf-socket escalation: live peer presence dark this long after the
/// tripwire probe → redial on a fresh socket (see `check_presence_deafness`).
const PRESENCE_DEAF_REDIAL_MS: i64 = 60_000;

/// State for the presence deafness tripwire. Presence heartbeats ride the
/// SAME socket and the same DO broadcast fan-out as row updates, so "peers I
/// was seeing live via this room all went dark at once" is a *delivery*
/// signal, and it is bounded (~45-60s) at zero server cost — every device
/// already heartbeats each 15s. The monotonic seen-cache and the relay
/// status probe deliberately keep devices *fresh-looking* through other
/// paths; they must never feed this tripwire (they'd mask exactly the
/// failure it exists to catch — 2026-08-04 deaf-socket incident).
#[derive(Default)]
struct PresenceWatch {
    /// Armed once at least one OTHER device has been seen live via the
    /// room's presence map this session.
    armed: bool,
    /// Epoch ms when live peers first all went dark (0 = not dark).
    dark_since_ms: i64,
    /// The cheap first response (probe) already fired.
    probed: bool,
}

/// Cheap decorrelation jitter (0–500ms) without pulling in a rng — derived from
/// the sub-nanosecond wall clock. Mirrors the device relay's `jitter()`.
pub(crate) fn join_retry_jitter() -> std::time::Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::time::Duration::from_millis(u64::from(nanos) % 500)
}

#[derive(Debug, Clone)]
pub struct WorkspaceHostConfig {
    pub device_id: String,
    /// Human name for this device's registry row (hostname by default).
    pub device_name: String,
    /// `std::env::consts::OS`-style platform string.
    pub platform: String,
    pub org_id: String,
    /// The signed-in user — registries are per-user (`reg1/{orgId}/{userId}`):
    /// spaces/sessions are private to their owner, never org-visible.
    pub user_id: String,
    /// When present, the host joins `/registry/{orgId}/ws`. `None` = fully offline
    /// (local snapshots only; the registry still drives everything device-side).
    pub edge: Option<EdgeConfig>,
}

struct WorkspaceHostInner {
    store: Arc<DocsStore>,
    config: WorkspaceHostConfig,
    reg: Arc<Mutex<RegistryDoc>>,
    chats_tx: watch::Sender<Vec<Chat>>,
    devices_tx: watch::Sender<Vec<Device>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    spaces_tx: watch::Sender<Vec<Space>>,
    room: Mutex<Option<Arc<RegistryClient>>>,
    /// Bumped on every registry change (local mutation or applied server
    /// frame) — drives republish + the snapshot debounce in `workspace_task`.
    changed_tx: watch::Sender<u64>,
    /// Freshest presence heartbeat (ms) we have EVER observed per device. The
    /// room's presence map forgets entries after its 30s TTL and starts empty
    /// on a (re)join, so without this cache a receive-side hiccup snaps a
    /// device's overlay back to its boot-time row `lastSeenAt` — an instant
    /// (and false) "offline" badge for a host that beat 20s ago.
    presence_seen: Mutex<std::collections::HashMap<String, i64>>,
    /// Called with a device id whenever its presence heartbeat proves it alive —
    /// wired to `LinkCache::reset_cooldown` so a peer that comes back is dialed
    /// immediately instead of waiting out the failure backoff.
    peer_alive: Mutex<Option<PeerAliveHook>>,
    /// Deaf-socket tripwire state — see `check_presence_deafness`.
    presence_watch: Mutex<PresenceWatch>,
    /// Epoch ms of the registry room's most recent (re)join — the dial gate's
    /// warm-up clock (`peer_liveness`): a just-joined room hasn't heard
    /// anyone's heartbeat yet, and that silence must not read as "offline".
    room_joined_at: std::sync::atomic::AtomicI64,
}

/// "This peer is alive" callback (device id) — see `WorkspaceHost::set_peer_alive_hook`.
pub type PeerAliveHook = Arc<dyn Fn(&str) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct WorkspaceHost {
    inner: Arc<WorkspaceHostInner>,
}

impl WorkspaceHost {
    /// Load (or migrate, or init) the registry, upsert this device's row, start
    /// the change-driven task, and join the edge registry room when configured.
    pub fn open(store: Arc<DocsStore>, config: WorkspaceHostConfig) -> Result<Self, EngineError> {
        let mut doc = match store.load_snapshot(REGISTRY_DOC_ID)? {
            Some(bytes) => RegistryDoc::from_bytes(&bytes, &config.device_id)
                .map_err(|e| EngineError::Other(format!("registry snapshot load failed: {e}")))?,
            None => {
                // MIGRATION (instant, one-time): seed from the legacy Loro
                // workspace snapshot when one exists. Seeds are pending upserts
                // with historical HLCs — the overlay serves the full sidebar
                // immediately, the room converges on first join, and any live
                // write beats a migrated value. The legacy snapshot stays on
                // disk for rollback.
                let mut doc = RegistryDoc::new(&config.device_id);
                match store.load_snapshot(WORKSPACE_DOC_ID) {
                    Ok(Some(bytes)) => {
                        let raw = loro::LoroDoc::new();
                        match raw.import(&bytes) {
                            Ok(_) => {
                                let legacy = WorkspaceDoc::from_doc(raw);
                                match legacy.read_all() {
                                    Ok(state) => match doc.seed_from_workspace(&state) {
                                        Ok(rows) => {
                                            tracing::info!(
                                                rows,
                                                "migrated legacy workspace doc into the registry"
                                            );
                                        }
                                        Err(err) => {
                                            tracing::warn!(error = %err, "workspace migration seed failed");
                                        }
                                    },
                                    Err(err) => {
                                        tracing::warn!(error = %err, "legacy workspace read failed; starting empty");
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "legacy workspace import failed; starting empty");
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "legacy workspace snapshot load failed; starting empty");
                    }
                }
                doc
            }
        };
        // Destructive-break hygiene: the pre-spaces row stays unreachable.
        store.delete_snapshot(LEGACY_WORKSPACE_DOC_ID).ok();

        // Boot: upsert our own device row. A user-set name (RenameDevice is LWW from
        // any device) survives restarts. The old fallback sentinel is repaired with
        // the platform-resolved name because it was never a user-selected name.
        let now = Utc::now();
        let existing = doc
            .read_devices()?
            .into_iter()
            .find(|d| d.id == config.device_id);
        doc.upsert_device(&Device {
            id: config.device_id.clone(),
            name: device_name_on_boot(
                existing.as_ref().map(|device| device.name.as_str()),
                &config.device_name,
            ),
            platform: config.platform.clone(),
            last_seen_at: Some(now),
            // First registration stamps `createdAt`; restarts keep the original
            // (the Devices page "Added …" fragment).
            created_at: existing.and_then(|d| d.created_at).or(Some(now)),
            // Every boot restamps the running binary's version (fleet staleness
            // on the Devices page; workspace version — same for every crate).
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        })?;

        let state = doc.read_all()?;
        let (chats_tx, _) = watch::channel(state.chats);
        let (devices_tx, _) = watch::channel(state.devices);
        let (sessions_tx, _) = watch::channel(state.sessions);
        let (spaces_tx, _) = watch::channel(state.spaces);
        let (changed_tx, changed_rx) = watch::channel(0u64);

        let host = Self {
            inner: Arc::new(WorkspaceHostInner {
                store,
                config,
                reg: Arc::new(Mutex::new(doc)),
                chats_tx,
                devices_tx,
                sessions_tx,
                spaces_tx,
                room: Mutex::new(None),
                changed_tx,
                presence_seen: Mutex::new(std::collections::HashMap::new()),
                peer_alive: Mutex::new(None),
                presence_watch: Mutex::new(PresenceWatch::default()),
                room_joined_at: std::sync::atomic::AtomicI64::new(0),
            }),
        };
        // Persist immediately: after this boot the migration source is never
        // read again, so the registry snapshot must exist even if the process
        // dies before the first debounced save.
        host.inner.save_snapshot();
        host.join_room();
        tokio::spawn(workspace_task(Arc::downgrade(&host.inner), changed_rx));
        if host.inner.config.edge.is_some() {
            tokio::spawn(relay_probe_task(Arc::downgrade(&host.inner)));
        }
        Ok(host)
    }

    /// Edge room join — offline-tolerant: a failed join logs and stays local-first.
    fn join_room(&self) {
        let Some(edge) = &self.inner.config.edge else {
            return;
        };
        let org_id = self.inner.config.org_id.clone();
        // Per-dial URL provider: the bearer is re-read on every (re)connect.
        let url = edge.room_url(format!("/registry/{org_id}/ws"));
        self.spawn_join(url, edge.token_changes(), Some(edge.token.clone()));
    }

    /// Test seam: join a registry room at a fixed WebSocket URL without an
    /// `EdgeConfig` — integration tests wire hosts to an in-process mock
    /// server through this. Production always goes through [`Self::join_room`].
    #[doc(hidden)]
    pub fn connect_registry_url(&self, url: &str) {
        self.spawn_join(Arc::new(zeron_sync::StaticUrl(url.to_string())), None, None);
    }

    fn spawn_join(
        &self,
        url: Arc<dyn zeron_sync::UrlProvider>,
        mut token_changes: Option<tokio::sync::watch::Receiver<u64>>,
        token: Option<Arc<dyn zeron_rpc::TokenSource>>,
    ) {
        let org_id = self.inner.config.org_id.clone();
        let reg = self.inner.reg.clone();
        let device_id = self.inner.config.device_id.clone();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut wake = zeron_sync::wake::subscribe();
            let mut online = zeron_sync::wake::subscribe_online();
            // `RegistryClient` only self-reconnects AFTER a first successful
            // join; an INITIAL failure (a 500 from an overloaded DO, a token
            // racing a refresh, an edge deploy) must not end this task and
            // leave the device offline until an app restart. Retry the first
            // join on a capped, jittered backoff so a transient blip self-heals.
            let mut backoff = JOIN_RETRY_BASE;
            loop {
                if weak.upgrade().is_none() {
                    return; // host dropped
                }
                let tuning = RegistryTuning {
                    probe_quiet: REGISTRY_PROBE_QUIET,
                };
                // Production path (token present): dual transport — the WS
                // dials as before, and a plain-HTTPS pull/push seam derived
                // from the same URL provider bootstraps the doc in ~1 RTT
                // and keeps syncing at backoff cadence when the socket can't
                // connect (airplane-wifi networks strip WS upgrades).
                let result = if token.is_some() {
                    RegistryClient::connect_via_transport(
                        url.clone(),
                        reg.clone(),
                        &device_id,
                        tuning,
                        Arc::new(WsDerivedRegistryTransport::new(url.clone())),
                    )
                    .await
                } else {
                    RegistryClient::connect_via_tuned(url.clone(), reg.clone(), &device_id, tuning)
                        .await
                };
                match result {
                    Ok(client) => {
                        let client = Arc::new(client);
                        client.set_presence(now_ms());
                        let mut events = client.events();
                        if token_revoked(&token).await {
                            return;
                        }
                        let Some(inner) = weak.upgrade() else { return };
                        *lock(&inner.room) = Some(client.clone());
                        inner
                            .room_joined_at
                            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                        inner.bump_changed();
                        tracing::info!(org = %org_id, "registry room joined");
                        drop(inner);
                        // The slot is the sole owner. This lets engine-level
                        // revocation close the socket synchronously by taking it.
                        drop(client);
                        // The event pump lives for the client's whole life
                        // (across its self-reconnects); it ends only when the
                        // client is dropped at host teardown.
                        loop {
                            tokio::select! {
                                event = events.recv() => match event {
                                    Ok(zeron_sync::RegistryEvent::Connected) => {
                                        let Some(inner) = weak.upgrade() else { return };
                                        // Re-join: restart the dial gate's
                                        // warm-up clock (presence map is empty
                                        // again; silence isn't evidence yet).
                                        inner
                                            .room_joined_at
                                            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                                        inner.bump_changed();
                                    }
                                    Ok(zeron_sync::RegistryEvent::Applied) => {
                                        let Some(inner) = weak.upgrade() else { return };
                                        inner.bump_changed();
                                    }
                                    Ok(zeron_sync::RegistryEvent::Presence) => {
                                        let Some(inner) = weak.upgrade() else { return };
                                        inner.publish();
                                    }
                                    Ok(zeron_sync::RegistryEvent::Disconnected) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                },
                                _ = token_changed(&mut token_changes) => {
                                    if token_revoked(&token).await {
                                        tracing::info!(org = %org_id,
                                            "registry credentials removed; leaving room");
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(inner) = weak.upgrade() {
                            *lock(&inner.room) = None;
                        }
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(org = %org_id, error = %err, backoff_ms = backoff.as_millis() as u64,
                            "registry room join failed; retrying");
                    }
                }
                // Drain stale online events so only successes DURING this
                // wait cut it short (our own failed dial doesn't count).
                while online.try_recv().is_ok() {}
                tokio::select! {
                    _ = tokio::time::sleep(backoff + join_retry_jitter()) => {
                        backoff = (backoff * 2).min(JOIN_RETRY_CAP);
                    }
                    _ = wake.recv() => {
                        backoff = JOIN_RETRY_BASE;
                    }
                    _ = online.recv() => {
                        backoff = JOIN_RETRY_BASE;
                    }
                    _ = token_changed(&mut token_changes) => {
                        if token_revoked(&token).await {
                            return;
                        }
                        backoff = JOIN_RETRY_BASE;
                    }
                }
            }
        });
    }

    /// Close the current registry membership before account-scoped state is
    /// drained. The auth signal prevents an in-flight join from replacing it.
    pub fn disconnect_edge(&self) {
        lock(&self.inner.room).take();
    }

    /// Wire the "peer is alive" signal (fresh presence heartbeat) to a callback —
    /// the engine points this at `LinkCache::reset_cooldown`.
    pub fn set_peer_alive_hook(&self, hook: PeerAliveHook) {
        *lock(&self.inner.peer_alive) = Some(hook);
    }

    /// Test seam: write a foreign device row (the relay version gate and the
    /// dial gate read these; production rows arrive via registry sync).
    #[doc(hidden)]
    pub fn upsert_device_row(&self, device: &Device) {
        if let Err(err) = self.mutate(|doc| doc.upsert_device(device)) {
            tracing::warn!(error = %err, "device row upsert failed");
        }
    }

    /// Dial-gate verdict for `LinkCache` (park dials to registry-dark peers).
    /// `Dark` requires positive evidence; every ambiguous state — registry
    /// room down or still in its warm-up window, unknown device, heartbeat in
    /// the gray zone — stays `Unknown`, because a dead registry room (rows
    /// down, relay fine: the 2026-08-18 03:45 incident shape) must never
    /// park the relay. Un-park is presence-driven: heartbeats resume → the
    /// verdict flips and the peer-alive hook clears any dial cooldown.
    pub fn peer_liveness(&self, device_id: &str) -> zeron_rpc::PeerLiveness {
        use zeron_rpc::PeerLiveness::{Dark, Live, Unknown};
        if device_id == self.inner.config.device_id {
            return Live;
        }
        let now = now_ms();
        let (connected, live_beat) = {
            let room = lock(&self.inner.room);
            match room.as_ref() {
                Some(room) => (
                    room.stats().connected,
                    room.presence().get(device_id).copied(),
                ),
                None => (false, None),
            }
        };
        if !connected {
            return Unknown;
        }
        let cached = lock(&self.inner.presence_seen).get(device_id).copied();
        match live_beat.into_iter().chain(cached).max() {
            Some(ms) if now.saturating_sub(ms) < PRESENCE_FRESH_MS => Live,
            Some(ms) if now.saturating_sub(ms) >= DIAL_GATE_DARK_MS => Dark,
            Some(_) => Unknown,
            None => {
                // Never heard this device beat. Silence is only evidence once
                // OUR room has been joined long enough to have heard it.
                let joined_at = self
                    .inner
                    .room_joined_at
                    .load(std::sync::atomic::Ordering::Relaxed);
                if joined_at == 0 || now.saturating_sub(joined_at) < DIAL_GATE_WARMUP_MS {
                    return Unknown;
                }
                // Fall back to the durable row: a device whose last stamped
                // sighting is ancient AND which isn't beating now is dark.
                let row_seen = self
                    .read(|doc| doc.read_devices())
                    .ok()
                    .into_iter()
                    .flatten()
                    .find(|d| d.id == device_id)
                    .and_then(|d| d.last_seen_at)
                    .map(|at| at.timestamp_millis());
                match row_seen {
                    Some(ms) if now.saturating_sub(ms) >= DIAL_GATE_DARK_MS => Dark,
                    _ => Unknown,
                }
            }
        }
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    pub fn connected(&self) -> bool {
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|room| room.stats().connected)
    }

    /// True once this replica has applied a server state this process (WS
    /// hello answer or HTTPS pull) — or when no edge is configured at all
    /// (local-only: the local table IS the whole truth). Destructive repair
    /// (the orphan sweep) gates on this: a replica that has not heard from
    /// the server this boot may be arbitrarily behind (gapped cursor,
    /// pre-heal, offline at boot), and "row absent locally" on such a view
    /// is not evidence of deletion.
    pub fn registry_synced(&self) -> bool {
        if self.inner.config.edge.is_none() {
            return true;
        }
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|room| room.stats().synced)
    }

    /// Probe the registry room's liveness NOW (window-focus sweep). Probes are
    /// deadline-checked in the client: an unanswered probe tears the session
    /// down for a fresh socket, so a deaf-receiving room (2026-08-04 incident)
    /// heals within seconds of the user looking at the app.
    pub fn probe(&self) {
        if let Some(room) = lock(&self.inner.room).as_ref() {
            room.probe();
        }
    }

    /// Registry room introspection for SyncStatus / `zeron sync`.
    /// `None` = no room yet (edge-less, or the initial join is still retrying).
    pub fn sync_status(&self) -> Option<zeron_sync::RoomStatsSnapshot> {
        lock(&self.inner.room).as_ref().map(|room| room.stats())
    }

    /// The registry room's reconnect posture (next-dial deadline + sticky
    /// last failure) for the connectivity stream. `None` = no room yet.
    pub fn reconnect_state(&self) -> Option<zeron_sync::ReconnectState> {
        lock(&self.inner.room)
            .as_ref()
            .map(|room| room.reconnect_state())
    }

    /// Whether this engine was configured with edge transports at all
    /// (`false` = local profile: connectivity is `Disabled`, hide the pill).
    pub fn edge_expected(&self) -> bool {
        self.inner.config.edge.is_some()
    }

    // ── registry access helpers ─────────────────────────────────────────────

    /// Run a mutation under the registry lock, then wake the publish/persist
    /// task and push the write to the room.
    fn mutate<R>(&self, f: impl FnOnce(&mut RegistryDoc) -> R) -> R {
        let result = f(&mut lock(&self.inner.reg));
        self.inner.bump_changed();
        if let Some(room) = lock(&self.inner.room).as_ref() {
            room.nudge();
        }
        result
    }

    fn read<R>(&self, f: impl FnOnce(&RegistryDoc) -> R) -> R {
        f(&lock(&self.inner.reg))
    }

    /// The chat row as currently known (overlay view).
    pub fn chat(&self, chat_id: &str) -> Result<Option<Chat>, EngineError> {
        Ok(self.read(|doc| doc.chat(chat_id))?)
    }

    /// The space row as currently known (overlay view).
    pub fn space(&self, space_id: &str) -> Result<Option<Space>, EngineError> {
        Ok(self.read(|doc| doc.space(space_id))?)
    }

    pub fn read_chats(&self) -> Result<Vec<Chat>, EngineError> {
        Ok(self.read(|doc| doc.read_chats())?)
    }

    pub fn read_devices(&self) -> Result<Vec<Device>, EngineError> {
        Ok(self.read(|doc| doc.read_devices())?)
    }

    pub fn read_sessions(&self) -> Result<Vec<Session>, EngineError> {
        Ok(self.read(|doc| doc.read_sessions())?)
    }

    // ── watches (WatchChats / WatchDevices / merged WatchSessions) ──────────

    pub fn watch_chats(&self) -> watch::Receiver<Vec<Chat>> {
        self.inner.chats_tx.subscribe()
    }

    pub fn watch_devices(&self) -> watch::Receiver<Vec<Device>> {
        self.inner.devices_tx.subscribe()
    }

    /// Raw workspace session-status rows (all devices').
    pub fn watch_session_rows(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn watch_spaces(&self) -> watch::Receiver<Vec<Space>> {
        self.inner.spaces_tx.subscribe()
    }

    /// WatchSessions source: remote devices' rows from the registry merged with
    /// this engine's live status watch (the local view is fresher for our own runs).
    pub fn merged_sessions_watch(
        &self,
        local: watch::Receiver<Vec<Session>>,
    ) -> watch::Receiver<Vec<Session>> {
        let mut rows = self.watch_session_rows();
        let mut local = local;
        let device_id = self.inner.config.device_id.clone();
        let (tx, rx) = watch::channel(merge_sessions(&device_id, &rows.borrow(), &local.borrow()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rows.changed() => if changed.is_err() { break },
                    changed = local.changed() => if changed.is_err() { break },
                }
                let merged = merge_sessions(
                    &device_id,
                    &rows.borrow_and_update(),
                    &local.borrow_and_update(),
                );
                if tx.send(merged).is_err() {
                    break; // no receivers left
                }
            }
        });
        rx
    }

    // ── chat ownership ──────────────────────────────────────────────────────

    /// Writer discipline: the chat's host is its row's `deviceId`. Unknown chats
    /// are claimable — the first run command claims them via [`Self::claim_chat`].
    pub fn is_host(&self, chat_id: &str) -> bool {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(Some(chat)) => chat.device_id == self.inner.config.device_id,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                true
            }
        }
    }

    /// Claim-on-first-command: create the chat row under OUR device id when a run
    /// command arrives for a chat with no row yet. No-op when the row exists.
    ///
    /// The claim is a PARTIAL row write (identity/cwd/space only): the command
    /// plane is nudged and outruns the registry channel, so the client's
    /// `createChat` for the same chat routinely arrives AFTER the claim with
    /// older clocks — fields the claim never wrote (`config`, `title`) must
    /// still land then.
    ///
    /// Spaces invariant: every chat belongs to a space, so the claim resolves an
    /// own-device space matching `cwd` — or auto-creates one (gitDetected false;
    /// SpacesSync corrects on its next pass). A cwd-less claim (e.g. note_message
    /// racing ahead of the run command) leaves `spaceId` unset; the row is
    /// invisible to the UI until a spaced claim/create lands.
    pub fn claim_chat(&self, chat_id: &str, cwd: Option<&str>) -> Result<(), EngineError> {
        if self.read(|doc| doc.chat(chat_id))?.is_some() {
            return Ok(());
        }
        let space_id = match cwd {
            Some(cwd) => Some(self.space_for_path(cwd)?),
            None => None,
        };
        self.mutate(|doc| doc.claim_chat(chat_id, cwd, space_id.as_deref(), Utc::now()));
        Ok(())
    }

    /// An own-device space whose path matches, else one at the path's parent
    /// checkout root, else a freshly created one at that root.
    ///
    /// A linked-worktree cwd resolves to the checkout root FIRST: claiming at
    /// the worktree path itself minted a phantom sidebar space named after the
    /// worktree folder ("clever-ember") next to the project's real space.
    fn space_for_path(&self, path: &str) -> Result<String, EngineError> {
        let device_id = &self.inner.config.device_id;
        let spaces = self.read(|doc| doc.read_spaces())?;
        if let Some(space) = spaces
            .iter()
            .find(|s| s.device_id == *device_id && s.path == path)
        {
            return Ok(space.id.clone());
        }
        let root = linked_worktree_root(std::path::Path::new(path));
        if let Some(root) = root.as_deref()
            && let Some(space) = spaces
                .iter()
                .find(|s| s.device_id == *device_id && s.path == root)
        {
            return Ok(space.id.clone());
        }
        let space = Space {
            id: crate::new_id(),
            device_id: device_id.clone(),
            path: root.unwrap_or_else(|| path.to_string()),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.mutate(|doc| doc.upsert_space(&space))?;
        Ok(space.id)
    }

    /// The chat's configured harness/model row, when present (RunRequest harness
    /// selection; callers fall back to the engine default).
    pub fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => chat.and_then(|c| c.config),
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                None
            }
        }
    }

    // ── host-side row writes ────────────────────────────────────────────────

    /// Sidebar freshness on message persist: preview = first 120 chars of the last
    /// message's text. Claims the row first so a pre-workspace chat gains one.
    pub fn note_message(&self, chat_id: &str, text: &str) {
        let preview: String = text.chars().take(120).collect();
        let result = self.claim_chat(chat_id, None).and_then(|_| {
            self.mutate(|doc| doc.set_chat_last_message(chat_id, &preview, Utc::now()))
                .map_err(EngineError::from)
        });
        if let Err(err) = result {
            tracing::warn!(chat = %chat_id, error = %err, "registry last-message write failed");
        }
    }

    /// Resume continuity: stamp the chat row with the harness-native session id
    /// of its latest run and the cwd it was created under. An empty `session_id`
    /// tombstones the row ("do not resume" after a rejected resume). Best-effort:
    /// a missing chat row (claim happens on first command) just returns.
    pub fn set_chat_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        match self.mutate(|doc| doc.set_chat_harness_session(chat_id, session_id, cwd)) {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry harness-session write failed");
            }
        }
    }

    /// The chat row's stored harness session `(session_id, cwd)`, if stamped.
    /// The empty-string tombstone passes through — callers must treat it as
    /// "explicitly no resume" (and must NOT fall back to older sources).
    pub fn chat_harness_session(&self, chat_id: &str) -> Option<(String, Option<String>)> {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => {
                let chat = chat?;
                let id = chat.harness_session_id?;
                Some((id, chat.harness_session_cwd))
            }
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                None
            }
        }
    }

    /// Session-status row upsert (sessions engine transitions land here too, in
    /// addition to the local watch channel).
    pub fn record_session(&self, session: &Session) {
        if let Err(err) = self.mutate(|doc| doc.upsert_session(session)) {
            tracing::warn!(chat = %session.chat_id, error = %err, "registry session write failed");
        }
    }

    // ── Mutate surface (LWW writes accepted from any device) ────────────────

    /// Create a chat, usually *in a project*: the project fixes the host device
    /// and base cwd (`cwd` override = an isolated-worktree path). With no
    /// `space_id` the chat is project-less: `device_id` picks the host and the
    /// cwd defaults to `~` (expanded host-side when the run spawns).
    pub fn create_chat(
        &self,
        chat_id: &str,
        space_id: Option<&str>,
        device_id: Option<&str>,
        config: Option<ChatConfig>,
        cwd: Option<String>,
    ) -> Result<(), EngineError> {
        if self.read(|doc| doc.chat(chat_id))?.is_some() {
            return Ok(()); // idempotent: optimistic client retries never duplicate
        }
        let space = match space_id {
            Some(space_id) => match self.read(|doc| doc.space(space_id))? {
                Some(space) => Some(space),
                None => return Err(EngineError::Other(format!("no such space: {space_id}"))),
            },
            None => None,
        };
        let host_device = match (&space, device_id) {
            (Some(space), _) => space.device_id.clone(),
            (None, Some(device_id)) => device_id.to_string(),
            (None, None) => {
                return Err(EngineError::Other(
                    "createChat needs a spaceId or a deviceId".into(),
                ));
            }
        };
        self.mutate(|doc| {
            doc.upsert_chat(&Chat {
                id: chat_id.to_string(),
                device_id: host_device.clone(),
                title: None,
                archived: false,
                cwd: Some(cwd.unwrap_or_else(|| {
                    space
                        .as_ref()
                        .map(|s| s.path.clone())
                        .unwrap_or_else(|| "~".to_string())
                })),
                branch: None,
                checkout_id: None,
                source_context: None,
                config,
                last_message_preview: None,
                last_message_at: None,
                created_at: Utc::now(),
                harness_session_id: None,
                // Born on chat2: a brand-new chat has an empty doc — nothing
                // to seed, no migration race to lose. Only pre-existing chats
                // go through the seed+flip path (the host migration sweep).
                room_gen: Some(2),
                harness_session_cwd: None,
                space_id: space.as_ref().map(|s| s.id.clone()),
                last_seen_at: None,
            })
        })?;
        Ok(())
    }

    // ── spaces (Mutate surface + owner stamps) ──────────────────────────────

    /// Create a space (any device). Idempotent by id; a live duplicate of the
    /// same `(deviceId, path)` is a no-op backstop (the UI reuses via
    /// WatchSpaces). `git_detected` is seeded from the picker's FolderEntry;
    /// the owning device's SpacesSync re-verifies.
    pub fn create_space(
        &self,
        space_id: &str,
        device_id: &str,
        path: &str,
        name: Option<String>,
        git_detected: bool,
    ) -> Result<(), EngineError> {
        let spaces = self.read(|doc| doc.read_spaces())?;
        if spaces
            .iter()
            .any(|s| s.id == space_id || (s.device_id == device_id && s.path == path))
        {
            return Ok(());
        }
        self.mutate(|doc| {
            doc.upsert_space(&Space {
                id: space_id.to_string(),
                device_id: device_id.to_string(),
                path: path.to_string(),
                name,
                git_detected,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            })
        })?;
        Ok(())
    }

    pub fn rename_space(&self, space_id: &str, name: Option<&str>) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_space(space_id, name))?)
    }

    /// Hard-delete a space and its chats (registry cascade — one atomic batch).
    /// The caller (rpc layer) tears down live runs / doc-host handles for the
    /// returned chat ids.
    pub fn delete_space(&self, space_id: &str) -> Result<DeletedSpace, EngineError> {
        Ok(self.mutate(|doc| doc.delete_space(space_id))?)
    }

    /// Synced seen marker (any device; LWW + monotonic guard in the doc layer).
    pub fn mark_chat_seen(
        &self,
        chat_id: &str,
        at: chrono::DateTime<Utc>,
    ) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_seen(chat_id, at))?)
    }

    /// Owner-only git stamp (SpacesSync). Refuses rows owned by another device.
    pub fn set_space_git(
        &self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
    ) -> Result<bool, EngineError> {
        match self.read(|doc| doc.space(space_id))? {
            Some(space) if space.device_id == self.inner.config.device_id => {
                Ok(self
                    .mutate(|doc| doc.set_space_git(space_id, detected, checkout_id, Utc::now()))?)
            }
            Some(space) => {
                tracing::warn!(
                    space = %space_id, owner = %space.device_id,
                    "refusing git stamp on space owned by another device"
                );
                Ok(false)
            }
            None => Ok(false),
        }
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, EngineError> {
        Ok(self.read(|doc| doc.read_spaces())?)
    }

    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_chat(chat_id, title))?)
    }

    /// Backdate a chat's activity timestamps (epoch ms). Returns false when
    /// the chat doesn't exist.
    pub fn set_chat_activity(
        &self,
        chat_id: &str,
        last_message_at: Option<i64>,
        created_at: Option<i64>,
    ) -> Result<bool, EngineError> {
        let Some(mut chat) = self.read(|doc| doc.chat(chat_id))? else {
            return Ok(false);
        };
        if let Some(ms) = last_message_at {
            chat.last_message_at = chrono::DateTime::<Utc>::from_timestamp_millis(ms);
        }
        if let Some(ms) = created_at
            && let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        {
            chat.created_at = at;
        }
        self.mutate(|doc| doc.upsert_chat(&chat))?;
        Ok(true)
    }

    /// Re-home a chat to another device (tooling/seeds; a future device
    /// migration flow will drive this). Returns false when the chat doesn't
    /// exist.
    pub fn set_chat_host(&self, chat_id: &str, device_id: &str) -> Result<bool, EngineError> {
        let Some(mut chat) = self.read(|doc| doc.chat(chat_id))? else {
            return Ok(false);
        };
        chat.device_id = device_id.to_string();
        self.mutate(|doc| doc.upsert_chat(&chat))?;
        Ok(true)
    }

    /// Upsert a chat row copied verbatim from another profile (local→synced
    /// import). Same write path as every live mutation, so the row persists
    /// and pushes like any other; the caller fixes `room_gen` beforehand.
    pub fn import_chat_row(&self, chat: &Chat) -> Result<(), EngineError> {
        Ok(self.mutate(|doc| doc.upsert_chat(chat))?)
    }

    /// Upsert a space row copied verbatim from another profile (local→synced
    /// import).
    pub fn import_space_row(&self, space: &Space) -> Result<(), EngineError> {
        Ok(self.mutate(|doc| doc.upsert_space(space))?)
    }

    /// Flip the chat's sync room generation (docs/chat2-sync.md M2) — the
    /// host calls this in the same breath as seeding the chat2 checkpoint.
    pub fn set_chat_room_gen(&self, chat_id: &str, room_gen: u32) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_room_gen(chat_id, room_gen))?)
    }

    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_archived(chat_id, archived))?)
    }

    /// LWW full-config replace on the chat row (zeron `SetChatConfig` — the
    /// composer's mid-session model/reasoning/options changes). Returns false
    /// when the chat doesn't exist.
    pub fn set_chat_config(&self, chat_id: &str, config: &ChatConfig) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_config(chat_id, config))?)
    }

    /// Tombstone: removes the chats (and session-status) row; the per-chat session
    /// doc remains untouched.
    pub fn delete_chat(&self, chat_id: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.delete_chat(chat_id))?)
    }

    pub fn rename_device(&self, device_id: &str, name: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_device(device_id, name))?)
    }

    // ── git metadata (diff-sync host writes) ────────────────────────────────

    /// HEAD-watcher reconciliation: the branch checked out at the chat's cwd.
    pub fn set_chat_branch(&self, chat_id: &str, branch: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_branch(chat_id, branch))?)
    }

    pub fn set_chat_source_context(
        &self,
        chat_id: &str,
        context: &zeron_proto::ConversationSourceContext,
    ) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_source_context(chat_id, context))?)
    }

    /// Retarget a chat onto another folder (mid-session switch to an existing
    /// worktree). Resume is cwd-scoped — the next run there starts fresh.
    pub fn set_chat_cwd(&self, chat_id: &str, cwd: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_cwd(chat_id, cwd))?)
    }

    /// Canonical checkout identity for the chat's cwd (diff grouping key).
    pub fn set_chat_checkout(&self, chat_id: &str, checkout_id: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_checkout(chat_id, checkout_id))?)
    }

    // ── persistence / teardown ──────────────────────────────────────────────

    /// Persist the snapshot now (shutdown path; bypasses the debounce).
    pub fn flush(&self) {
        self.inner.save_snapshot();
    }

    /// Shutdown: stamp our `lastSeenAt` (the only periodic-ish row write besides
    /// boot) and flush the snapshot.
    pub fn shutdown(&self) {
        let now = Utc::now();
        let device_id = self.inner.config.device_id.clone();
        if let Err(err) = self.mutate(|doc| doc.set_device_last_seen(&device_id, now)) {
            tracing::warn!(error = %err, "device lastSeenAt stamp failed");
        }
        self.inner.save_snapshot();
    }
}

impl WorkspaceHostInner {
    fn bump_changed(&self) {
        self.changed_tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn publish(&self) {
        self.publish_lists(false);
    }

    fn publish_lists(&self, clock_tick: bool) {
        match lock(&self.reg).read_all() {
            Ok(mut state) => {
                self.overlay_presence(&mut state.devices);
                // Retain the latest value even with no subscribers, but don't
                // wake every list for unrelated registry/presence changes.
                publish_if_changed(&self.chats_tx, state.chats);
                // The periodic presence tick also drives time-based expiry.
                // Unrelated registry writes need no synthetic device event.
                if clock_tick {
                    self.devices_tx.send_replace(state.devices);
                } else {
                    publish_if_changed(&self.devices_tx, state.devices);
                }
                publish_if_changed(&self.sessions_tx, state.sessions);
                publish_if_changed(&self.spaces_tx, state.spaces);
            }
            Err(err) => {
                tracing::warn!(error = %err, "registry read failed");
            }
        }
    }

    /// Fold the 15s presence heartbeats into the device rows' `lastSeenAt`
    /// before publishing. The row is written on boot/shutdown ONLY (server-
    /// state hygiene), so without this overlay every device looks offline
    /// ~70s after its boot — and a genuinely dead host is indistinguishable
    /// from slow sync. Fresh remote heartbeats also fire the peer-alive hook
    /// (dial-cooldown reset).
    fn overlay_presence(&self, devices: &mut [Device]) {
        let mut alive_peers: Vec<String> = Vec::new();
        {
            // No live room handle is NOT "everyone is offline": the cache (fed
            // by past heartbeats and the relay-status probe) still overlays —
            // a dead registry room must never fake an offline badge for
            // devices whose relay connection is fine.
            let room = lock(&self.room);
            let live_map = room
                .as_ref()
                .map(|room| room.presence())
                .unwrap_or_default();
            let mut seen = lock(&self.presence_seen);
            let now = now_ms();
            let mut live_fresh_peers = 0usize;
            for device in devices.iter_mut() {
                // Freshest of the live presence entry and the cache: the room
                // map's 30s TTL (and its empty state right after a rejoin)
                // must not erase freshness this engine already witnessed — the
                // device is offline only once heartbeats genuinely stop
                // arriving for the UI's whole online window.
                let live = live_map.get(&device.id).copied();
                if device.id != self.config.device_id
                    && live.is_some_and(|ms| now.saturating_sub(ms) < PRESENCE_FRESH_MS)
                {
                    live_fresh_peers += 1;
                }
                let cached = seen.get(&device.id).copied();
                let Some(ms) = live.into_iter().chain(cached).max() else {
                    continue;
                };
                seen.insert(device.id.clone(), ms);
                if let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(ms)
                    && device.last_seen_at.is_none_or(|prev| prev < at)
                {
                    device.last_seen_at = Some(at);
                }
                if device.id != self.config.device_id && now.saturating_sub(ms) < PRESENCE_FRESH_MS
                {
                    alive_peers.push(device.id.clone());
                }
            }
            if let Some(room) = room.as_ref() {
                self.check_presence_deafness(room, live_fresh_peers, now);
            }
        }
        if alive_peers.is_empty() {
            return;
        }
        let hook = lock(&self.peer_alive).clone();
        if let Some(hook) = hook {
            for id in &alive_peers {
                hook(id);
            }
        }
    }

    /// The deaf-socket tripwire (see [`PresenceWatch`]). LIVE presence
    /// freshness only — never the seen-cache or relay probe. Escalation
    /// ladder: first all-dark observation → deadline-checked probe (free on a
    /// healthy room); still dark [`PRESENCE_DEAF_REDIAL_MS`] later → fresh-
    /// socket redial (the only cure when the server→client path drops even
    /// probe answers). Disarms after the redial and re-arms when a peer is
    /// next seen live, so a genuinely-offline fleet costs one probe + one
    /// redial, ever.
    fn check_presence_deafness(&self, room: &RegistryClient, live_fresh_peers: usize, now: i64) {
        let mut watch = lock(&self.presence_watch);
        if live_fresh_peers > 0 {
            watch.armed = true;
            watch.dark_since_ms = 0;
            watch.probed = false;
            return;
        }
        if !watch.armed {
            return;
        }
        if watch.dark_since_ms == 0 {
            watch.dark_since_ms = now;
        }
        if !watch.probed {
            tracing::info!(
                "all live peer presence went dark; probing registry room (deaf-socket tripwire)"
            );
            room.probe();
            watch.probed = true;
        } else if now.saturating_sub(watch.dark_since_ms) > PRESENCE_DEAF_REDIAL_MS {
            tracing::warn!(
                dark_ms = now.saturating_sub(watch.dark_since_ms),
                "peer presence still dark after probe; requesting registry room redial"
            );
            room.redial();
            watch.armed = false;
            watch.dark_since_ms = 0;
            watch.probed = false;
        }
    }

    fn save_snapshot(&self) {
        let bytes = lock(&self.reg).to_bytes();
        match bytes {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot(REGISTRY_DOC_ID, &bytes) {
                    tracing::warn!(error = %err, "registry snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "registry snapshot export failed");
            }
        }
    }

    /// Presence heartbeat — a memory-only frame on the room, never a row write.
    fn presence_tick(&self) {
        if let Some(room) = lock(&self.room).as_ref() {
            room.set_presence(now_ms());
        }
    }
}

/// The parent checkout root of a linked git worktree: `<path>/.git` is a FILE
/// containing `gitdir: <root>/.git/worktrees/<name>`. `None` for a primary
/// checkout (`.git` is a directory), a non-repo folder, or any other layout
/// (bare-repo worktrees have no `<root>` working copy to attribute to). Pure
/// fs reads — no git subprocess; this runs on the synchronous claim path.
pub(crate) fn linked_worktree_root(path: &std::path::Path) -> Option<String> {
    let gitfile = path.join(".git");
    if !std::fs::metadata(&gitfile).ok()?.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&gitfile).ok()?;
    let target = content
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let mut target = std::path::PathBuf::from(target);
    if target.is_relative() {
        // Rare (`worktree.useRelativePaths`); canonicalize resolves the
        // `../..` hops against the real filesystem.
        target = std::fs::canonicalize(path.join(target)).ok()?;
    }
    let worktrees = target.parent()?;
    let dot_git = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || dot_git.file_name()? != ".git" {
        return None;
    }
    Some(dot_git.parent()?.to_string_lossy().into_owned())
}

/// Local live statuses win for this device's chats; every other device's rows come
/// from the registry. Sorted by chat id (stable stream output).
fn merge_sessions(device_id: &str, rows: &[Session], local: &[Session]) -> Vec<Session> {
    let mut merged: std::collections::HashMap<String, Session> = rows
        .iter()
        .filter(|s| s.device_id != device_id)
        .map(|s| (s.chat_id.clone(), s.clone()))
        .collect();
    for session in local {
        merged.insert(session.chat_id.clone(), session.clone());
    }
    let mut list: Vec<Session> = merged.into_values().collect();
    list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
    list
}

/// Background task: relay-verified presence. Every [`RELAY_PROBE_INTERVAL_MS`],
/// for each known device whose merged heartbeat freshness has gone stale, ask
/// its DeviceRoom whether the host socket is live (`/device/{id}/status`); a
/// positive answer refreshes the presence cache so the overlay keeps the badge
/// online. The DeviceRoom shares no machinery with the registry room, so a
/// false "offline" now requires BOTH independent paths to be down — at which
/// point the device is, for every purpose the app has, genuinely offline.
/// Steady state (healthy room, fresh heartbeats) probes nothing.
async fn relay_probe_task(weak: Weak<WorkspaceHostInner>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(RELAY_PROBE_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    let client = reqwest::Client::new();
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        let Some(edge) = inner.config.edge.clone() else {
            return;
        };
        let self_id = inner.config.device_id.clone();
        let now = now_ms();
        let stale: Vec<String> = {
            let Ok(devices) = lock(&inner.reg).read_devices() else {
                continue;
            };
            let seen = lock(&inner.presence_seen);
            devices
                .into_iter()
                .filter(|d| d.id != self_id)
                .filter(|d| {
                    seen.get(&d.id)
                        .is_none_or(|ms| now.saturating_sub(*ms) >= PRESENCE_FRESH_MS)
                })
                .map(|d| d.id)
                .collect()
        };
        drop(inner);
        if stale.is_empty() {
            continue;
        }
        let Some(bearer) = edge.bearer().await else {
            continue; // signed out
        };
        let mut refreshed = false;
        for device_id in stale {
            let url = format!(
                "{}/device/{}/status",
                edge.url.trim_end_matches('/'),
                device_id
            );
            let response = client
                .get(&url)
                .bearer_auth(&bearer)
                .timeout(RELAY_PROBE_TIMEOUT)
                .send()
                .await;
            let Ok(response) = response else { continue };
            if !response.status().is_success() {
                continue;
            }
            let Ok(body) = response.json::<serde_json::Value>().await else {
                continue;
            };
            if body
                .get("hostConnected")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                let Some(inner) = weak.upgrade() else { return };
                lock(&inner.presence_seen).insert(device_id.clone(), now_ms());
                tracing::debug!(device = %device_id, "presence: relay-verified alive");
                refreshed = true;
            }
        }
        if refreshed && let Some(inner) = weak.upgrade() {
            inner.publish();
        }
    }
}

/// Background task: reacts to registry changes (local mutations and applied
/// server frames) by re-publishing the watch channels and debouncing snapshots,
/// and refreshes presence every [`PRESENCE_INTERVAL_MS`]. Holds only a weak
/// handle so a dropped host tears the task down.
async fn workspace_task(weak: Weak<WorkspaceHostInner>, mut changed_rx: watch::Receiver<u64>) {
    let mut presence =
        tokio::time::interval(std::time::Duration::from_millis(PRESENCE_INTERVAL_MS));
    presence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    presence.tick().await; // consume the immediate first tick
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // host (and its change sender) is gone
                }
                let Some(inner) = weak.upgrade() else { break };
                inner.publish();
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(inner) = weak.upgrade() else { break };
                inner.save_snapshot();
            }
            _ = presence.tick() => {
                let Some(inner) = weak.upgrade() else { break };
                inner.presence_tick();
                // Re-publish on the same cadence: remote heartbeats decay when a
                // device goes silent, and watchers (the UI online dot, "host
                // offline" hints) need a tick to observe that staleness.
                inner.publish_lists(true);
            }
        }
    }
}

fn publish_if_changed<T: PartialEq>(sender: &watch::Sender<T>, value: T) {
    sender.send_if_modified(|current| {
        if *current == value {
            false
        } else {
            *current = value;
            true
        }
    });
}

fn device_name_on_boot(existing_name: Option<&str>, detected_name: &str) -> String {
    existing_name
        .filter(|name| {
            let name = name.trim();
            !name.is_empty() && name != crate::LEGACY_UNKNOWN_DEVICE_NAME
        })
        .unwrap_or(detected_name)
        .to_string()
}

/// Plain-HTTPS registry pull/push derived from the SAME WebSocket URL
/// provider the dial uses (`wss://…/registry/{org}/ws?token=…&device=…`):
/// swap the scheme, swap the `/ws` leaf for `/rows` or `/push`, keep the
/// fresh token+device the provider already mints per attempt. No second
/// auth path to maintain, and the `?beat=1` keeps presence alive for a
/// device that can only reach the edge over HTTPS.
struct WsDerivedRegistryTransport {
    url: Arc<dyn zeron_sync::UrlProvider>,
    client: reqwest::Client,
}

impl WsDerivedRegistryTransport {
    fn new(url: Arc<dyn zeron_sync::UrlProvider>) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }

    async fn leaf_url(
        provider: &Arc<dyn zeron_sync::UrlProvider>,
        leaf: &str,
    ) -> Result<(reqwest::Url, Option<String>), zeron_sync::SyncError> {
        let ws = provider.url().await?;
        let mut u = reqwest::Url::parse(&ws)
            .map_err(|e| zeron_sync::SyncError::Protocol(format!("bad ws url: {e}")))?;
        let scheme = if u.scheme() == "wss" { "https" } else { "http" };
        let _ = u.set_scheme(scheme);
        let path = u.path().to_string();
        let Some(base) = path.strip_suffix("/ws") else {
            return Err(zeron_sync::SyncError::Protocol(
                "ws url without /ws leaf".into(),
            ));
        };
        let new_path = format!("{base}/{leaf}");
        u.set_path(&new_path);
        // Move the WS URL's ?token= into a bearer header (HTTP supports
        // headers; query strings can reach request logs). Other params
        // (device) stay.
        let token = u
            .query_pairs()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned());
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| k != "token")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        u.query_pairs_mut().clear();
        for (k, v) in kept {
            u.query_pairs_mut().append_pair(&k, &v);
        }
        Ok((u, token))
    }
}

impl zeron_sync::RegistryTransport for WsDerivedRegistryTransport {
    fn fetch(
        &self,
        since: u64,
    ) -> futures::future::BoxFuture<'static, Result<String, zeron_sync::SyncError>> {
        let provider = self.url.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let (mut u, token) = Self::leaf_url(&provider, "rows").await?;
            u.query_pairs_mut()
                .append_pair("since", &since.to_string())
                .append_pair("beat", "1");
            let mut req = client.get(u);
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| zeron_sync::SyncError::WebSocket(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(zeron_sync::SyncError::Protocol(format!(
                    "registry pull http {}",
                    resp.status()
                )));
            }
            resp.text()
                .await
                .map_err(|e| zeron_sync::SyncError::WebSocket(e.to_string()))
        })
    }

    fn push(
        &self,
        body: String,
    ) -> futures::future::BoxFuture<'static, Result<String, zeron_sync::SyncError>> {
        let provider = self.url.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let (u, token) = Self::leaf_url(&provider, "push").await?;
            let mut req = client
                .post(u)
                .header("content-type", "application/json")
                .body(body);
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| zeron_sync::SyncError::WebSocket(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(zeron_sync::SyncError::Protocol(format!(
                    "registry push http {}",
                    resp.status()
                )));
            }
            resp.text()
                .await
                .map_err(|e| zeron_sync::SyncError::WebSocket(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{device_name_on_boot, linked_worktree_root};

    #[tokio::test]
    async fn presence_publish_keeps_unchanged_lists_quiet_and_late_subscribers_current() {
        use super::*;

        let dir = tempfile::tempdir().unwrap();
        let host = WorkspaceHost::open(
            Arc::new(DocsStore::open(dir.path()).unwrap()),
            WorkspaceHostConfig {
                device_id: "test-device".into(),
                device_name: "Test device".into(),
                platform: "macos".into(),
                org_id: "test-org".into(),
                user_id: "test-user".into(),
                edge: None,
            },
        )
        .unwrap();
        let chats = host.watch_chats();
        let sessions = host.watch_session_rows();
        let mut spaces = host.watch_spaces();
        let devices = host.watch_devices();

        host.inner.publish();
        assert!(!chats.has_changed().unwrap());
        assert!(!sessions.has_changed().unwrap());
        assert!(!spaces.has_changed().unwrap());
        assert!(!devices.has_changed().unwrap());
        host.inner.publish_lists(true);
        assert!(
            devices.has_changed().unwrap(),
            "presence expiry still ticks"
        );
        assert!(!chats.has_changed().unwrap());
        assert!(!sessions.has_changed().unwrap());
        assert!(!spaces.has_changed().unwrap());

        host.create_space("space", "test-device", "/project", None, false)
            .unwrap();
        host.inner.publish();
        assert!(
            spaces.has_changed().unwrap(),
            "real changes must wake readers"
        );
        assert_eq!(spaces.borrow_and_update()[0].id, "space");
        assert!(!chats.has_changed().unwrap());
        assert!(!sessions.has_changed().unwrap());
        drop(spaces);

        host.rename_space("space", Some("Renamed")).unwrap();
        host.inner.publish();
        assert_eq!(
            host.watch_spaces().borrow()[0].name.as_deref(),
            Some("Renamed")
        );
    }

    #[test]
    fn boot_repairs_the_legacy_unknown_device_sentinel() {
        assert_eq!(
            device_name_on_boot(Some("unknown-device"), "MacBook Pro"),
            "MacBook Pro"
        );
    }

    #[test]
    fn boot_preserves_a_user_selected_device_name() {
        assert_eq!(
            device_name_on_boot(Some("Work laptop"), "MacBook Pro"),
            "Work laptop"
        );
    }

    #[test]
    fn linked_worktree_resolves_to_the_checkout_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let wt = dir.path().join("clever-ember");
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("clever-ember")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                root.join(".git/worktrees/clever-ember").display()
            ),
        )
        .unwrap();
        assert_eq!(
            linked_worktree_root(&wt).as_deref(),
            Some(root.to_str().unwrap())
        );
    }

    #[test]
    fn primary_checkouts_and_plain_folders_resolve_to_none() {
        let dir = tempfile::tempdir().unwrap();
        // Primary checkout: `.git` is a directory.
        let primary = dir.path().join("primary");
        std::fs::create_dir_all(primary.join(".git")).unwrap();
        assert_eq!(linked_worktree_root(&primary), None);
        // Not a repo at all.
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(linked_worktree_root(&plain), None);
        // A `.git` file pointing somewhere that is not `<root>/.git/worktrees/<name>`.
        let odd = dir.path().join("odd");
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::write(odd.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        assert_eq!(linked_worktree_root(&odd), None);
    }
}
