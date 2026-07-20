//! WorkspaceHost — owns the per-org `WorkspaceDoc` (ARCHITECTURE §2.2): local snapshot
//! persistence (doc id `"workspace"`), edge room sync (`ws/{orgId}`, offline-tolerant),
//! the device registry row for THIS device, and the typed watch channels the
//! WatchChats/WatchDevices/WatchSessions RPC streams are fed from.
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

use comet_doc::{WorkspaceDoc, presence_key};
use comet_proto::{Chat, ChatConfig, Device, Session};
use comet_sync::{DocsStore, RoomClient};

use crate::doc_host::EdgeConfig;
use crate::{EngineError, now_ms};

/// Snapshot row id in the local `DocsStore` (chat ids never collide with it).
pub const WORKSPACE_DOC_ID: &str = "workspace";
/// Org used when none is configured (matches the edge's dev-mode `user@org` bearers).
pub const DEFAULT_ORG_ID: &str = "dev-org";
/// Ephemeral presence refresh cadence.
const PRESENCE_INTERVAL_MS: u64 = 15_000;
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
    room: Mutex<Option<RoomClient>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

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

        let host = Self {
            inner: Arc::new(WorkspaceHostInner {
                store,
                config,
                doc,
                chats_tx,
                devices_tx,
                sessions_tx,
                room: Mutex::new(None),
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
        let ws_base = edge.url.replacen("http", "ws", 1);
        let org_id = self.inner.config.org_id.clone();
        let url = format!("{}/workspace/{}/ws?token={}", ws_base, org_id, edge.token);
        let room_id = format!("ws/{org_id}");
        let room_doc = self.inner.doc.doc().clone();
        let device_id = self.inner.config.device_id.clone();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            match RoomClient::connect(&url, &room_id, room_doc).await {
                Ok(client) => {
                    client.ephemeral().set(&presence_key(&device_id), now_ms());
                    if let Some(inner) = weak.upgrade() {
                        *lock(&inner.room) = Some(client);
                        tracing::info!(room = %room_id, "workspace room joined");
                    }
                }
                Err(err) => {
                    tracing::warn!(room = %room_id, error = %err, "workspace room join failed; staying offline");
                }
            }
        });
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
    pub fn claim_chat(&self, chat_id: &str, cwd: Option<&str>) -> Result<(), EngineError> {
        if self.inner.doc.chat(chat_id)?.is_some() {
            return Ok(());
        }
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
        })?;
        Ok(())
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

    /// Session-status row upsert (sessions engine transitions land here too, in
    /// addition to the local watch channel).
    pub fn record_session(&self, session: &Session) {
        if let Err(err) = self.inner.doc.upsert_session(session) {
            tracing::warn!(chat = %session.chat_id, error = %err, "workspace session write failed");
        }
    }

    // ── Mutate surface (LWW writes accepted from any device) ────────────────

    pub fn create_chat(
        &self,
        chat_id: &str,
        device_id: &str,
        config: Option<ChatConfig>,
        cwd: Option<String>,
    ) -> Result<(), EngineError> {
        if self.inner.doc.chat(chat_id)?.is_some() {
            return Ok(()); // idempotent: optimistic client retries never duplicate
        }
        self.inner.doc.upsert_chat(&Chat {
            id: chat_id.to_string(),
            device_id: device_id.to_string(),
            title: None,
            archived: false,
            cwd,
            branch: None,
            checkout_id: None,
            config,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
        })?;
        Ok(())
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
            Ok(state) => {
                // send_replace, NOT send: `watch::Sender::send` drops the value when
                // no receiver exists yet, so a stream subscribed later would start
                // from a stale snapshot (found the hard way by the e2e smoke).
                self.chats_tx.send_replace(state.chats);
                self.devices_tx.send_replace(state.devices);
                self.sessions_tx.send_replace(state.sessions);
            }
            Err(err) => {
                tracing::warn!(error = %err, "workspace read failed");
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
            }
        }
    }
}
