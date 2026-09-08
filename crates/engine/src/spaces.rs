//! SpacesSync — owner-side upkeep of space rows (git presence) plus the
//! orphan-chat repair sweep.
//!
//! A space is a synced (device, folder) pair; the folder need NOT be a git
//! repo. This service watches the workspace `spaces` rows owned by THIS device
//! and keeps their `gitDetected`/`checkoutId` stamps truthful:
//!
//! - recheck on boot / when a space row is first observed;
//! - a non-recursive `notify` watcher on the space folder — `.git` appearing or
//!   vanishing (git init / de-git) kicks a recheck;
//! - a slow 2-minute repair tick (native watchers coalesce/drop events).
//!
//! Stamps are written ONLY on change, so steady state never grows the oplog.
//! Remote devices read `space.git_detected` straight from the doc — branch
//! pickers and the diff sidebar gate on it with zero RPCs.
//!
//! The repair tick also runs the orphan sweep: a chat created concurrently
//! with a `deleteSpace` on another device can sync in after the cascade ran,
//! leaving a dangling `spaceId`. The HOST device deletes its own such chats
//! (writer discipline — we never touch other devices' rows). The sweep only
//! runs once the registry has applied a server state this boot
//! ([`WorkspaceHost::registry_synced`]): on an un-synced replica a missing
//! space row means "we haven't heard yet", not "deleted".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use zeron_proto::Space;

use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: recheck every owned space + orphan sweep.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);

struct SpaceEntry {
    path: PathBuf,
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the folder watcher alive; dropped on entry close. Filled
    /// asynchronously — FSEvents registration blocks, so [`reconcile`] builds
    /// it off the runtime and attaches it here once ready.
    folder_watch: Mutex<Option<notify::RecommendedWatcher>>,
}

struct SpacesSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    entries: Mutex<HashMap<String, Arc<SpaceEntry>>>,
    /// Ends the supervisor loop eagerly on shutdown (weak refs alone only end
    /// it once the whole graph drops).
    cancel: CancellationToken,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SpacesSync {
    inner: Arc<SpacesSyncInner>,
}

impl SpacesSync {
    /// Build and start the sync loop: follows the workspace spaces watch and
    /// runs the repair tick. Requires a tokio runtime.
    pub fn start(repos: Repos, workspace: WorkspaceHost, device_id: &str) -> Self {
        let sync = Self {
            inner: Arc::new(SpacesSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                entries: Mutex::new(HashMap::new()),
                cancel: CancellationToken::new(),
                supervisor: Mutex::new(None),
            }),
        };
        let task = tokio::spawn(spaces_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_spaces(),
            sync.inner.cancel.clone(),
        ));
        *lock(&sync.inner.supervisor) = Some(task);
        sync
    }

    /// Stop the supervisor loop and wait for it to exit (per-space tasks are
    /// purely local and end when their entries drop). Idempotent.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let task = lock(&self.inner.supervisor).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Reconcile + recheck now (tests / opportunistic callers).
    pub async fn reconcile_now(&self) {
        let spaces = self.inner.workspace.watch_spaces().borrow().clone();
        reconcile(&self.inner, &spaces);
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }
}

/// (Re)build the entry set for the spaces THIS device owns.
fn reconcile(inner: &Arc<SpacesSyncInner>, spaces: &[Space]) {
    let owned: HashMap<&str, &Space> = spaces
        .iter()
        .filter(|s| s.device_id == inner.device_id)
        .map(|s| (s.id.as_str(), s))
        .collect();

    let mut entries = lock(&inner.entries);
    entries.retain(|id, _| owned.contains_key(id.as_str()));
    for (id, space) in owned {
        if entries.contains_key(id) {
            continue; // deviceId/path are immutable — nothing to refresh
        }
        let (kick_tx, kick_rx) = mpsc::unbounded_channel();
        let entry = Arc::new(SpaceEntry {
            path: PathBuf::from(&space.path),
            kick_tx: kick_tx.clone(),
            folder_watch: Mutex::new(None),
        });
        entries.insert(id.to_string(), entry.clone());
        tokio::spawn(entry_task(
            Arc::downgrade(inner),
            id.to_string(),
            Arc::downgrade(&entry),
            kick_rx,
        ));
        let _ = kick_tx.send(()); // initial check (boot / first observed)

        // Non-recursive watcher on the space folder: `.git` appearing/vanishing
        // among the direct children is exactly the signal we need. Watch
        // failures are fine — the repair tick still converges. Built off the
        // runtime: FSEvents registration blocks, and reconcile runs on the
        // spaces-watch task.
        let weak = Arc::downgrade(&entry);
        tokio::task::spawn_blocking(move || {
            let Some(entry) = weak.upgrade() else {
                return; // entry removed before the watcher was ready
            };
            let tx = entry.kick_tx.clone();
            let result =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    let Ok(event) = event else { return };
                    if event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|n| n == ".git"))
                    {
                        let _ = tx.send(());
                    }
                });
            match result {
                Ok(mut watcher) => {
                    use notify::Watcher as _;
                    match watcher.watch(&entry.path, notify::RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            *lock(&entry.folder_watch) = Some(watcher);
                            // Close the check→attach gap: a `.git` change while
                            // unwatched gets caught by this recheck.
                            let _ = entry.kick_tx.send(());
                        }
                        Err(err) => {
                            tracing::debug!(path = %entry.path.display(), error = %err, "spaces: watch failed");
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "spaces: watcher create failed");
                }
            }
        });
    }
}

/// Per-space task: trailing-debounce kicks, then recheck git presence.
async fn entry_task(
    inner: Weak<SpacesSyncInner>,
    space_id: String,
    entry: Weak<SpaceEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
) {
    while kick_rx.recv().await.is_some() {
        loop {
            match tokio::time::timeout(WATCH_DEBOUNCE, kick_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return, // entry closed mid-burst
                Err(_) => break,
            }
        }
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else {
            return;
        };
        check_space(&inner, &space_id, &entry.path).await;
    }
}

/// Probe git presence and stamp the row — write only on change.
async fn check_space(inner: &Arc<SpacesSyncInner>, space_id: &str, path: &Path) {
    let detected = inner.repos.is_repo(path).await;
    let checkout_id = if detected {
        match inner.repos.checkout_identity(path).await {
            Ok(identity) => Some(identity.id),
            Err(err) => {
                tracing::debug!(space = %space_id, error = %err, "spaces: checkout identity failed");
                None
            }
        }
    } else {
        None
    };
    let current = match inner.workspace.read_spaces() {
        Ok(spaces) => spaces.into_iter().find(|s| s.id == space_id),
        Err(err) => {
            tracing::warn!(space = %space_id, error = %err, "spaces: row read failed");
            return;
        }
    };
    let Some(current) = current else {
        return; // deleted while checking
    };
    if current.git_detected == detected && current.checkout_id == checkout_id {
        return; // unchanged — no oplog growth
    }
    match inner
        .workspace
        .set_space_git(space_id, detected, checkout_id.as_deref())
    {
        Ok(_) => {
            tracing::info!(space = %space_id, git = detected, "space git presence updated");
        }
        Err(err) => {
            tracing::warn!(space = %space_id, error = %err, "spaces: git stamp failed");
        }
    }
}

/// Host-side repair: delete OUR chats whose `spaceId` dangles (create-vs-delete
/// race). Chats hosted by other devices are left alone.
fn sweep_orphans(inner: &Arc<SpacesSyncInner>) {
    // NEVER sweep off a replica that hasn't heard from the server this boot:
    // a gapped/pre-heal/offline-at-boot view is missing rows, and "space row
    // absent" on it is not evidence the space was deleted — sweeping there
    // globally destroys live chats (2026-08-23 missing-sessions audit).
    if !inner.workspace.registry_synced() {
        tracing::debug!("spaces: orphan sweep skipped (registry not yet synced this boot)");
        return;
    }
    let spaces = inner.workspace.watch_spaces().borrow().clone();
    let live: std::collections::HashSet<&str> = spaces.iter().map(|s| s.id.as_str()).collect();
    let chats = inner.workspace.watch_chats().borrow().clone();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(space_id) = chat.space_id.as_deref() else {
            continue;
        };
        if live.contains(space_id) {
            continue;
        }
        tracing::info!(chat = %chat.id, space = %space_id, "deleting orphaned chat (space gone)");
        if let Err(err) = inner.workspace.delete_chat(&chat.id) {
            tracing::warn!(chat = %chat.id, error = %err, "spaces: orphan delete failed");
        }
    }
}

/// Spaces-watch follower + repair tick. Weak handles so dropping the service
/// tears the loop down; the token ends it eagerly on shutdown.
async fn spaces_task(
    inner: Weak<SpacesSyncInner>,
    mut spaces_rx: watch::Receiver<Vec<Space>>,
    cancel: CancellationToken,
) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    {
        let Some(inner) = inner.upgrade() else { return };
        let spaces = spaces_rx.borrow().clone();
        reconcile(&inner, &spaces);
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            changed = spaces_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let spaces = spaces_rx.borrow_and_update().clone();
                reconcile(&inner, &spaces);
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let spaces = spaces_rx.borrow().clone();
                reconcile(&inner, &spaces);
                for entry in lock(&inner.entries).values() {
                    let _ = entry.kick_tx.send(());
                }
                sweep_orphans(&inner);
            }
        }
    }
}
