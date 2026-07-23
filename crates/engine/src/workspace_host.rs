//! WorkspaceHost — owns the per-user `WorkspaceDoc` (ARCHITECTURE §2.2, made
//! per-user for privacy): local snapshot persistence, edge room sync
//! (`ws3/{orgId}/{userId}`, offline-tolerant — spaces/sessions are private to
//! their owner, never org-visible), the device registry row for THIS device,
//! and the typed watch channels the WatchChats/WatchDevices/WatchSessions RPC
//! streams are fed from.
//!
//! Writer discipline (kept from the doc schema): this host writes its own device row,
//! its own session-status rows, and rows for chats it hosts; renames/archives are LWW
//! sets accepted from any device (the Mutate surface).
//!
//! Liveness: `lastSeenAt` is a map write on boot/shutdown ONLY — the periodic 15s
//! heartbeat rides the room's `EphemeralStore` (`presence/{deviceId}` → timestamp), so
//! staying online never grows the workspace oplog.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use chrono::Utc;
use tokio::sync::watch;

use comet_doc::{DeletedSpace, WorkspaceDoc, presence_key};
use comet_proto::{Chat, ChatConfig, Device, Session, Space};
use comet_sync::{DocsStore, RoomClient};

use crate::doc_host::EdgeConfig;
use crate::{EngineError, now_ms};

/// Snapshot row id in the local `DocsStore` (chat ids never collide with it).
/// `workspace2` = the spaces-overhaul destructive break: the legacy `workspace`
/// row is simply never read again. (The per-user room break — `ws2/{orgId}` →
/// `ws3/{orgId}/{userId}` — needed no row-id bump: the local store itself moved
/// to `orgs/{org}/{user}/`, so the old snapshot is unreachable anyway.)
pub const WORKSPACE_DOC_ID: &str = "workspace2";
/// Legacy (pre-spaces) snapshot row — best-effort deleted on open.
const LEGACY_WORKSPACE_DOC_ID: &str = "workspace";
/// Org used when none is configured (matches the edge's dev-mode `user@org` bearers).
pub const DEFAULT_ORG_ID: &str = "dev-org";
/// User used when none is configured (dev mode without a bearer).
pub const DEFAULT_USER_ID: &str = "dev-user";
/// Ephemeral presence refresh cadence.
const PRESENCE_INTERVAL_MS: u64 = 15_000;
/// A presence heartbeat younger than this marks the device alive (3 missed
/// beats = offline). Also the "peer is reachable" signal that clears the
/// peer-dial cooldown.
const PRESENCE_FRESH_MS: i64 = 45_000;
/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

#[derive(Debug, Clone)]
pub struct WorkspaceHostConfig {
    pub device_id: String,
    /// Human name for this device's registry row (hostname by default).
    pub device_name: String,
    /// `std::env::consts::OS`-style platform string.
    pub platform: String,
    pub org_id: String,
    /// The signed-in user — workspace docs are per-user (`ws3/{orgId}/{userId}`):
    /// spaces/sessions are private to their owner, never org-visible.
    pub user_id: String,
    /// When present, the host joins `/workspace/{orgId}/ws`. `None` = fully offline
    /// (local snapshots only; the doc still drives everything device-side).
    pub edge: Option<EdgeConfig>,
}

struct WorkspaceHostInner {
    store: Arc<DocsStore>,
    config: WorkspaceHostConfig,
    doc: Arc<WorkspaceDoc>,
    chats_tx: watch::Sender<Vec<Chat>>,
    devices_tx: watch::Sender<Vec<Device>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    spaces_tx: watch::Sender<Vec<Space>>,
    room: Mutex<Option<RoomClient>>,
    /// Called with a device id whenever its presence heartbeat proves it alive —
    /// wired to `LinkCache::reset_cooldown` so a peer that comes back is dialed
    /// immediately instead of waiting out the failure backoff.
    peer_alive: Mutex<Option<PeerAliveHook>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
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
    /// Load (or init) the workspace doc, upsert this device's registry row, start the
    /// change-driven task, and join the edge workspace room when configured.
    pub fn open(store: Arc<DocsStore>, config: WorkspaceHostConfig) -> Result<Self, EngineError> {
        let doc = match store.load_snapshot(WORKSPACE_DOC_ID)? {
            Some(bytes) => {
                let raw = loro::LoroDoc::new();
                raw.import(&bytes).map_err(|e| {
                    EngineError::Other(format!("workspace snapshot import failed: {e}"))
                })?;
                WorkspaceDoc::from_doc(raw)
            }
            None => WorkspaceDoc::new(),
        };
        let doc = Arc::new(doc);
        // Destructive-break hygiene: drop the unreachable legacy snapshot row and
        // stamp the in-band schema version for the NEXT break to detect.
        store.delete_snapshot(LEGACY_WORKSPACE_DOC_ID).ok();
        doc.ensure_schema_version()?;

        // Boot: upsert our own device row. A user-set name (RenameDevice is LWW from
        // any device) survives restarts — only a missing row gets the hostname.
        let now = Utc::now();
        let existing = doc
            .read_devices()?
            .into_iter()
            .find(|d| d.id == config.device_id);
        doc.upsert_device(&Device {
            id: config.device_id.clone(),
            name: existing
                .as_ref()
                .map(|d| d.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| config.device_name.clone()),
            platform: config.platform.clone(),
            last_seen_at: Some(now),
            // First registration stamps `createdAt`; restarts keep the original
            // (the Devices page "Added …" fragment).
            created_at: existing.and_then(|d| d.created_at).or(Some(now)),
        })?;

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        let state = doc.read_all()?;
        let (chats_tx, _) = watch::channel(state.chats);
        let (devices_tx, _) = watch::channel(state.devices);
        let (sessions_tx, _) = watch::channel(state.sessions);
        let (spaces_tx, _) = watch::channel(state.spaces);

        let host = Self {
            inner: Arc::new(WorkspaceHostInner {
                store,
                config,
                doc,
                chats_tx,
                devices_tx,
                sessions_tx,
                spaces_tx,
                room: Mutex::new(None),
                peer_alive: Mutex::new(None),
                _sub: sub,
            }),
        };
        host.join_room();
        tokio::spawn(workspace_task(Arc::downgrade(&host.inner), changed_rx));
        Ok(host)
    }

    /// Edge room join — offline-tolerant: a failed join logs and stays local-first.
    fn join_room(&self) {
        let Some(edge) = &self.inner.config.edge else {
            return;
        };
        let org_id = self.inner.config.org_id.clone();
        // Per-dial URL provider: the bearer is re-read on every (re)connect.
        let url = edge.room_url(format!("/workspace/{org_id}/ws"));
        // `ws3/{orgId}/{userId}` = the per-user privacy room (must match the
        // edge's join id, which it derives from the caller's own auth claim —
        // a mismatched user can never join).
        let room_id = format!("ws3/{}/{}", org_id, self.inner.config.user_id);
        let room_doc = self.inner.doc.doc().clone();
        let device_id = self.inner.config.device_id.clone();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            match RoomClient::connect_via(url, &room_id, room_doc).await {
                Ok(client) => {
                    client.ephemeral().set(&presence_key(&device_id), now_ms());
                    let mut events = client.events();
                    if let Some(inner) = weak.upgrade() {
                        *lock(&inner.room) = Some(client);
                        tracing::info!(room = %room_id, "workspace room joined");
                    }
                    // Presence rides `%EPH`, never the doc — remote heartbeats
                    // must re-publish the device watch themselves (this is the
                    // signal that distinguishes "host offline" from slow sync).
                    loop {
                        match events.recv().await {
                            Ok(comet_sync::RoomEvent::EphemeralUpdate) => {
                                let Some(inner) = weak.upgrade() else { break };
                                inner.publish();
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(room = %room_id, error = %err, "workspace room join failed; staying offline");
                }
            }
        });
    }

    /// Wire the "peer is alive" signal (fresh presence heartbeat) to a callback —
    /// the engine points this at `LinkCache::reset_cooldown`.
    pub fn set_peer_alive_hook(&self, hook: PeerAliveHook) {
        *lock(&self.inner.peer_alive) = Some(hook);
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    pub fn doc(&self) -> &WorkspaceDoc {
        &self.inner.doc
    }

    pub fn doc_arc(&self) -> Arc<WorkspaceDoc> {
        self.inner.doc.clone()
    }

    pub fn connected(&self) -> bool {
        lock(&self.inner.room).is_some()
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

    /// WatchSessions source: remote devices' rows from the workspace doc merged with
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

    // ── chat ownership (replaces the M2 "host everything" pragmatism) ───────

    /// §2.2 writer discipline: the chat's host is its row's `deviceId`. Unknown chats
    /// are claimable — the first run command claims them via [`Self::claim_chat`].
    pub fn is_host(&self, chat_id: &str) -> bool {
        match self.inner.doc.chat(chat_id) {
            Ok(Some(chat)) => chat.device_id == self.inner.config.device_id,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                true
            }
        }
    }

    /// Claim-on-first-command: create the chat row under OUR device id when a run
    /// command arrives for a chat with no row yet. No-op when the row exists.
    ///
    /// Spaces invariant: every chat belongs to a space, so the claim resolves an
    /// own-device space matching `cwd` — or auto-creates one (gitDetected false;
    /// SpacesSync corrects on its next pass). A cwd-less claim (e.g. note_message
    /// racing ahead of the run command) leaves `spaceId` unset; the row is
    /// invisible to the UI until a spaced claim/create lands. NOTE: a worktree
    /// cwd claims a space *at the worktree path*, not the repo root — acceptable
    /// for tooling-only (raw doc command) traffic.
    pub fn claim_chat(&self, chat_id: &str, cwd: Option<&str>) -> Result<(), EngineError> {
        if self.inner.doc.chat(chat_id)?.is_some() {
            return Ok(());
        }
        let space_id = match cwd {
            Some(cwd) => Some(self.space_for_path(cwd)?),
            None => None,
        };
        self.inner.doc.upsert_chat(&Chat {
            id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            title: None,
            archived: false,
            cwd: cwd.map(str::to_string),
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id,
            last_seen_at: None,
        })?;
        Ok(())
    }

    /// An own-device space whose path matches, else a freshly created one.
    fn space_for_path(&self, path: &str) -> Result<String, EngineError> {
        let device_id = &self.inner.config.device_id;
        if let Some(space) = self
            .inner
            .doc
            .read_spaces()?
            .into_iter()
            .find(|s| s.device_id == *device_id && s.path == path)
        {
            return Ok(space.id);
        }
        let space = Space {
            id: crate::new_id(),
            device_id: device_id.clone(),
            path: path.to_string(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.inner.doc.upsert_space(&space)?;
        Ok(space.id)
    }

    /// The chat's configured harness/model row, when present (RunRequest harness
    /// selection; callers fall back to the engine default).
    pub fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        match self.inner.doc.chat(chat_id) {
            Ok(chat) => chat.and_then(|c| c.config),
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
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
            self.inner
                .doc
                .set_chat_last_message(chat_id, &preview, Utc::now())
                .map_err(EngineError::from)
        });
        if let Err(err) = result {
            tracing::warn!(chat = %chat_id, error = %err, "workspace last-message write failed");
        }
    }

    /// Resume continuity: stamp the chat row with the harness-native session id
    /// of its latest run and the cwd it was created under (comet's
    /// `orbit.setChatHarnessSession`, sessions.ts:1039). An empty `session_id`
    /// tombstones the row ("do not resume" after a rejected resume). Best-effort:
    /// a missing chat row (claim happens on first command) just returns.
    pub fn set_chat_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        match self.inner.doc.set_chat_harness_session(chat_id, session_id, cwd) {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace harness-session write failed");
            }
        }
    }

    /// The chat row's stored harness session `(session_id, cwd)`, if stamped.
    /// The empty-string tombstone passes through — callers must treat it as
    /// "explicitly no resume" (and must NOT fall back to older sources).
    pub fn chat_harness_session(&self, chat_id: &str) -> Option<(String, Option<String>)> {
        match self.inner.doc.chat(chat_id) {
            Ok(chat) => {
                let chat = chat?;
                let id = chat.harness_session_id?;
                Some((id, chat.harness_session_cwd))
            }
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                None
            }
        }
    }

    /// Session-status row upsert (sessions engine transitions land here too, in
    /// addition to the local watch channel).
    pub fn record_session(&self, session: &Session) {
        if let Err(err) = self.inner.doc.upsert_session(session) {
            tracing::warn!(chat = %session.chat_id, error = %err, "workspace session write failed");
        }
    }

    // ── Mutate surface (LWW writes accepted from any device) ────────────────

    /// Create a chat *in a space*: the space fixes the host device and base cwd
    /// (`cwd` override = an isolated-worktree path). Fails when the space row is
    /// missing — the UI always creates chats from a picked space.
    pub fn create_chat(
        &self,
        chat_id: &str,
        space_id: &str,
        config: Option<ChatConfig>,
        cwd: Option<String>,
    ) -> Result<(), EngineError> {
        if self.inner.doc.chat(chat_id)?.is_some() {
            return Ok(()); // idempotent: optimistic client retries never duplicate
        }
        let Some(space) = self.inner.doc.space(space_id)? else {
            return Err(EngineError::Other(format!("no such space: {space_id}")));
        };
        self.inner.doc.upsert_chat(&Chat {
            id: chat_id.to_string(),
            device_id: space.device_id.clone(),
            title: None,
            archived: false,
            cwd: Some(cwd.unwrap_or_else(|| space.path.clone())),
            branch: None,
            checkout_id: None,
            config,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some(space.id),
            last_seen_at: None,
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
        let spaces = self.inner.doc.read_spaces()?;
        if spaces
            .iter()
            .any(|s| s.id == space_id || (s.device_id == device_id && s.path == path))
        {
            return Ok(());
        }
        self.inner.doc.upsert_space(&Space {
            id: space_id.to_string(),
            device_id: device_id.to_string(),
            path: path.to_string(),
            name,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        })?;
        Ok(())
    }

    pub fn rename_space(&self, space_id: &str, name: Option<&str>) -> Result<bool, EngineError> {
        Ok(self.inner.doc.rename_space(space_id, name)?)
    }

    /// Hard-delete a space and its chats (doc cascade). The caller (rpc layer)
    /// tears down live runs / doc-host handles for the returned chat ids.
    pub fn delete_space(&self, space_id: &str) -> Result<DeletedSpace, EngineError> {
        Ok(self.inner.doc.delete_space(space_id)?)
    }

    /// Synced seen marker (any device; LWW + monotonic guard in the doc layer).
    pub fn mark_chat_seen(&self, chat_id: &str, at: chrono::DateTime<Utc>) -> Result<bool, EngineError> {
        Ok(self.inner.doc.set_chat_seen(chat_id, at)?)
    }

    /// Owner-only git stamp (SpacesSync). Refuses rows owned by another device.
    pub fn set_space_git(
        &self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
    ) -> Result<bool, EngineError> {
        match self.inner.doc.space(space_id)? {
            Some(space) if space.device_id == self.inner.config.device_id => Ok(self
                .inner
                .doc
                .set_space_git(space_id, detected, checkout_id, Utc::now())?),
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
        Ok(self.inner.doc.read_spaces()?)
    }

    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc.rename_chat(chat_id, title)?)
    }

    /// Backdate a chat's activity timestamps (epoch ms). Returns false when
    /// the chat doesn't exist.
    pub fn set_chat_activity(
        &self,
        chat_id: &str,
        last_message_at: Option<i64>,
        created_at: Option<i64>,
    ) -> Result<bool, EngineError> {
        let Some(mut chat) = self.inner.doc.chat(chat_id)? else {
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
        self.inner.doc.upsert_chat(&chat)?;
        Ok(true)
    }

    /// Re-home a chat to another device (tooling/seeds; a future device
    /// migration flow will drive this). Returns false when the chat doesn't
    /// exist.
    pub fn set_chat_host(&self, chat_id: &str, device_id: &str) -> Result<bool, EngineError> {
        let Some(mut chat) = self.inner.doc.chat(chat_id)? else {
            return Ok(false);
        };
        chat.device_id = device_id.to_string();
        self.inner.doc.upsert_chat(&chat)?;
        Ok(true)
    }


    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) -> Result<bool, EngineError> {
        Ok(self.inner.doc.set_chat_archived(chat_id, archived)?)
    }

    /// LWW full-config replace on the chat row (comet `SetChatConfig` — the
    /// composer's mid-session model/reasoning/options changes). Returns false
    /// when the chat doesn't exist.
    pub fn set_chat_config(&self, chat_id: &str, config: &ChatConfig) -> Result<bool, EngineError> {
        Ok(self.inner.doc.set_chat_config(chat_id, config)?)
    }

    /// Tombstone: removes the chats (and session-status) row; the per-chat session
    /// doc remains untouched.
    pub fn delete_chat(&self, chat_id: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc.delete_chat(chat_id)?)
    }

    pub fn rename_device(&self, device_id: &str, name: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc.rename_device(device_id, name)?)
    }

    // ── git metadata (diff-sync host writes) ────────────────────────────────

    /// HEAD-watcher reconciliation: the branch checked out at the chat's cwd.
    pub fn set_chat_branch(&self, chat_id: &str, branch: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc.set_chat_branch(chat_id, branch)?)
    }

    /// Retarget a chat onto another folder (mid-session switch to an existing
    /// worktree). Resume is cwd-scoped — the next run there starts fresh.
    pub fn set_chat_cwd(&self, chat_id: &str, cwd: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc.set_chat_cwd(chat_id, cwd)?)
    }

    /// Canonical checkout identity for the chat's cwd (diff grouping key).
    pub fn set_chat_checkout(&self, chat_id: &str, checkout_id: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc.set_chat_checkout(chat_id, checkout_id)?)
    }

    // ── persistence / teardown ──────────────────────────────────────────────

    /// Persist the snapshot now (shutdown path; bypasses the debounce).
    pub fn flush(&self) {
        self.inner.save_snapshot();
    }

    /// Shutdown: stamp our `lastSeenAt` (the only periodic-ish map write besides
    /// boot) and flush the snapshot.
    pub fn shutdown(&self) {
        let now = Utc::now();
        if let Err(err) = self
            .inner
            .doc
            .set_device_last_seen(&self.inner.config.device_id, now)
        {
            tracing::warn!(error = %err, "device lastSeenAt stamp failed");
        }
        self.inner.save_snapshot();
    }
}

impl WorkspaceHostInner {
    fn publish(&self) {
        match self.doc.read_all() {
            Ok(mut state) => {
                self.overlay_presence(&mut state.devices);
                // send_replace, NOT send: `watch::Sender::send` drops the value when
                // no receiver exists yet, so a stream subscribed later would start
                // from a stale snapshot (found the hard way by the e2e smoke).
                self.chats_tx.send_replace(state.chats);
                self.devices_tx.send_replace(state.devices);
                self.sessions_tx.send_replace(state.sessions);
                self.spaces_tx.send_replace(state.spaces);
            }
            Err(err) => {
                tracing::warn!(error = %err, "workspace read failed");
            }
        }
    }

    /// Fold the 15s ephemeral presence heartbeats into the device rows'
    /// `lastSeenAt` before publishing. The doc row is written on boot/shutdown
    /// ONLY (oplog hygiene), so without this overlay every device looks offline
    /// ~70s after its boot — and a genuinely dead host is indistinguishable
    /// from slow sync. Fresh remote heartbeats also fire the peer-alive hook
    /// (dial-cooldown reset).
    fn overlay_presence(&self, devices: &mut [Device]) {
        let mut alive_peers: Vec<String> = Vec::new();
        {
            let room = lock(&self.room);
            let Some(room) = room.as_ref() else { return };
            let now = now_ms();
            for device in devices.iter_mut() {
                let Some(loro::LoroValue::I64(ms)) =
                    room.ephemeral().get(&presence_key(&device.id))
                else {
                    continue;
                };
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

    fn save_snapshot(&self) {
        match self.doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot(WORKSPACE_DOC_ID, &bytes) {
                    tracing::warn!(error = %err, "workspace snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "workspace snapshot export failed");
            }
        }
    }

    /// Ephemeral presence heartbeat — relayed over `%EPH`, never the oplog.
    fn presence_tick(&self) {
        if let Some(room) = lock(&self.room).as_ref() {
            room.ephemeral()
                .set(&presence_key(&self.config.device_id), now_ms());
        }
    }
}

/// Local live statuses win for this device's chats; every other device's rows come
/// from the workspace doc. Sorted by chat id (stable stream output).
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

/// Background task: reacts to doc changes (local commits and remote imports) by
/// re-publishing the watch channels and debouncing snapshots, and refreshes ephemeral
/// presence every [`PRESENCE_INTERVAL_MS`]. Holds only a weak handle so a dropped
/// host tears the task down.
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
                inner.publish();
            }
        }
    }
}
