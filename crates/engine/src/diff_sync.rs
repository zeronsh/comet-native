//! CheckoutDiffSync — checkout-scoped working-tree diff production (feature-inventory
//! §3.5; port of zeron's `checkout-diff-sync.ts` + `git-metadata-sync.ts`).
//!
//! Chats do not own working-tree state: a concrete Git checkout does. This service
//! groups this device's chats by their canonical checkout identity (`chat.cwd` →
//! [`Repos::checkout_identity`]), computes one bounded atomic snapshot per checkout,
//! and publishes it three ways:
//!
//! - the local `WatchCheckoutDiffs` stream (a watch channel of every checkout's
//!   latest [`CheckoutDiff`]);
//! - a [`DiffSidecar`] JSON `POST {edge}/diff/{chatId}` for every syncing chat of
//!   the checkout (bearer = engine edge token), so "review pending changes while
//!   the host sleeps" works;
//! Checkout snapshots remain live checkout state. Conversation branch identity
//! is captured by the command host and is never rewritten from this watcher;
//! otherwise one checkout change would relabel every chat sharing that folder.
//!
//! Fast recursive `notify` watchers (debounced [`WATCH_DEBOUNCE`]) are backed by a
//! slow 2-minute repair tick because native watchers may coalesce or drop events.
//! Snapshots carry a sha256 checksum; an unchanged checksum publishes nothing.
//!
//! Reconcile is deliberately damped, because it runs on *every* workspace chat
//! row change — including the `checkoutId` writes reconcile itself makes.
//! Checkout identities are memoized per cwd (chat-watch reconciles spawn
//! no git; the repair tick revalidates), and an entry whose chats vanish is torn
//! down only after [`REPAIR_INTERVAL`] of continuous absence rather than on the
//! first pass that misses it. See [`resolve_identity`] for the incident this
//! guards against.
//!
//! Beyond the working-tree watch, the service also answers one-shot
//! `GetCheckoutDiff` captures for the Changes pane's scopes: *branch changes*
//! (vs `merge-base(baseRef, HEAD)`, same capture path with the base overridden)
//! and *latest turn* (vs a temp-index `write-tree` snapshot taken when a turn
//! dispatches — see [`CheckoutDiffSync::note_turn_start`]).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use zeron_proto::{Chat, CheckoutDiff, DiffFileSummary};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::repos::{CheckoutIdentity, Repos};
use crate::workspace_host::WorkspaceHost;

/// Hard cap on the unified patch (plus untracked hunks) — "Partial snapshot".
pub const MAX_PATCH_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_DIFF_SOURCE_BYTES: usize = 2 * 1024 * 1024;
/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: re-reconcile + re-sync every checkout.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);
/// Max subdirectories a checkout may have before we skip its live recursive
/// watch (one OS watch per dir; past this the watcher thread's own bookkeeping
/// costs more than instant diffs are worth). A normal source tree is well
/// under this; a node_modules/vendored tree blows past it. The repair tick
/// still covers skipped checkouts.
const MAX_WATCH_DIRS: usize = 8_000;
/// `git hash-object -t tree /dev/null` — diff base for repos with no commits yet.
const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Latest-only diff sidecar published to each chat's session DO slot
/// (`POST /diff/{chatId}`; shape: edge/src/session-doc/sidecar.ts).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSidecar {
    pub chat_id: String,
    pub device_id: String,
    pub checkout_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    /// Epoch millis.
    pub published_at: i64,
}

/// One bounded atomic snapshot of a checkout's working tree.
#[derive(Debug, Clone)]
pub struct DiffSnapshot {
    pub branch: String,
    pub head_sha: Option<String>,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    pub checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFileTextPair {
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub old_content_hash: Option<String>,
    pub new_content_hash: Option<String>,
    pub binary: bool,
    pub truncated: bool,
}

struct CheckoutEntry {
    identity: CheckoutIdentity,
    chats: Mutex<Vec<Chat>>,
    /// Last published checksum — unchanged snapshots publish nothing.
    checksum: Mutex<Option<String>>,
    /// Set when a reconcile pass finds no chats for this checkout; cleared the
    /// moment chats reappear. The entry (watchers, checksum state, published
    /// diff) is only torn down after `orphan_grace` of *continuous* absence:
    /// tearing down and re-adding restarts a full capture, which costs seconds
    /// of CPU on a big checkout, so a single flapping chat-watch emission or
    /// transient identity failure must never destroy a live entry.
    orphaned_since: Mutex<Option<std::time::Instant>>,
    /// Kick channel into the entry's debounce/sync task.
    kick_tx: mpsc::UnboundedSender<()>,
    /// Destructive mutations are serialized per checkout. File-system
    /// watchers and read-only captures may still run concurrently.
    discard_lock: tokio::sync::Mutex<()>,
    /// Keeps the recursive fs watchers alive; dropped on entry close. Filled
    /// asynchronously — watcher setup (budget walk + FSEvents registration) can
    /// block for seconds, so [`add_entry`] does it off the runtime and attaches
    /// the result here once ready.
    watchers: Mutex<Vec<notify::RecommendedWatcher>>,
}

/// Working-tree snapshot recorded when a chat's turn dispatches — the diff
/// base for the Changes pane's "Latest turn" scope. In-memory only: after an
/// engine restart the scope is unavailable until the next turn.
#[derive(Debug, Clone)]
pub struct TurnSnapshot {
    /// Canonical checkout root the tree was captured in.
    pub root: PathBuf,
    /// `git write-tree` sha of the tracked + untracked (unignored) tree.
    pub tree: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

struct DiffSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
    entries: Mutex<HashMap<String, Arc<CheckoutEntry>>>,
    /// Serializes [`reconcile`] passes. Concurrent passes (chat-watch task vs.
    /// `reconcile_now`) can both observe a checkout as missing and both
    /// `add_entry` it — the second insert silently replaces the first entry,
    /// discarding its checksum state and kicking a redundant full capture.
    reconcile_gate: tokio::sync::Mutex<()>,
    /// cwd → resolved checkout identity. See [`resolve_identity`].
    identities: Mutex<HashMap<String, CheckoutIdentity>>,
    /// How long an entry may sit chat-less before reconcile removes it.
    orphan_grace: Duration,
    diffs_tx: watch::Sender<Vec<CheckoutDiff>>,
    /// chat_id → turn-start tree (see [`TurnSnapshot`]).
    turn_trees: Mutex<HashMap<String, TurnSnapshot>>,
    /// The tasks hold `Weak` refs, but an in-flight iteration holds an
    /// upgraded Arc — the token cuts it so no sidecar HTTP outlives shutdown.
    cancel: CancellationToken,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct CheckoutDiffSync {
    inner: Arc<DiffSyncInner>,
}

impl CheckoutDiffSync {
    /// Build and start the sync loop: follows the workspace chat watch and runs the
    /// 2-minute repair tick. Requires a tokio runtime.
    pub fn start(
        repos: Repos,
        workspace: WorkspaceHost,
        device_id: &str,
        edge: Option<EdgeConfig>,
    ) -> Self {
        // Grace = one repair interval: an entry must survive at least one full
        // fresh revalidation pass before reconcile may tear it down.
        Self::start_with_orphan_grace(repos, workspace, device_id, edge, REPAIR_INTERVAL)
    }

    /// [`CheckoutDiffSync::start`] with an explicit orphan grace — test hook so
    /// removal-after-grace is exercisable without waiting out [`REPAIR_INTERVAL`].
    #[doc(hidden)]
    pub fn start_with_orphan_grace(
        repos: Repos,
        workspace: WorkspaceHost,
        device_id: &str,
        edge: Option<EdgeConfig>,
        orphan_grace: Duration,
    ) -> Self {
        let (diffs_tx, _) = watch::channel(Vec::new());
        let sync = Self {
            inner: Arc::new(DiffSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                edge,
                http: reqwest::Client::new(),
                entries: Mutex::new(HashMap::new()),
                reconcile_gate: tokio::sync::Mutex::new(()),
                identities: Mutex::new(HashMap::new()),
                orphan_grace,
                diffs_tx,
                turn_trees: Mutex::new(HashMap::new()),
                cancel: CancellationToken::new(),
                supervisor: Mutex::new(None),
            }),
        };
        let task = tokio::spawn(diff_sync_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_chats(),
            sync.inner.cancel.clone(),
        ));
        *lock(&sync.inner.supervisor) = Some(task);
        sync
    }

    /// Stop the sync graph and wait for the supervisor to exit. Per-entry
    /// tasks observe the same token, so an in-flight `sync_entry` drops its
    /// sidecar POST instead of finishing it under a replaced runtime.
    /// Idempotent.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let task = lock(&self.inner.supervisor).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// `WatchCheckoutDiffs` source: every tracked checkout's latest diff.
    pub fn watch_diffs(&self) -> watch::Receiver<Vec<CheckoutDiff>> {
        self.inner.diffs_tx.subscribe()
    }

    /// Regroup this device's chats by checkout identity, then (re)build watchers.
    /// Public for tests (the background task calls it on every chat change).
    pub async fn reconcile_now(&self) {
        let chats = self.inner.workspace.watch_chats().borrow().clone();
        reconcile(&self.inner, chats, false).await;
    }

    /// Repair-tick path for tests: fresh identity revalidation, then kick every
    /// tracked checkout.
    #[doc(hidden)]
    pub async fn repair_now(&self) {
        let chats = self.inner.workspace.watch_chats().borrow().clone();
        reconcile(&self.inner, chats, true).await;
        self.sync_all();
    }

    /// Kick an immediate sync of every tracked checkout (repair-tick path).
    pub fn sync_all(&self) {
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }

    /// A turn is starting for `chat_id` in `cwd`: snapshot the checkout's tree
    /// in the background so "Latest turn" has a base. Best-effort — failures
    /// only log; a chat outside a checkout simply records nothing.
    pub fn note_turn_start(&self, chat_id: &str, cwd: &str) {
        let inner = Arc::downgrade(&self.inner);
        let chat_id = chat_id.to_string();
        let cwd = PathBuf::from(cwd);
        tokio::spawn(async move {
            let Some(inner) = inner.upgrade() else { return };
            let identity = match inner.repos.checkout_identity(&cwd).await {
                Ok(identity) => identity,
                Err(_) => return, // not a checkout
            };
            match snapshot_tree(&identity.root).await {
                Ok(tree) => {
                    lock(&inner.turn_trees).insert(
                        chat_id,
                        TurnSnapshot {
                            root: identity.root,
                            tree,
                            at: chrono::Utc::now(),
                        },
                    );
                }
                Err(err) => {
                    tracing::debug!(chat = %chat_id, error = %err,
                        "diff-sync: turn snapshot failed");
                }
            }
        });
    }

    /// The recorded turn-start snapshot for a chat, if any turn dispatched
    /// since boot.
    pub fn turn_snapshot(&self, chat_id: &str) -> Option<TurnSnapshot> {
        lock(&self.inner.turn_trees).get(chat_id).cloned()
    }

    /// Discard the complete uncommitted state for a tracked checkout after
    /// verifying that the UI acted on the latest full snapshot.
    pub async fn discard_working_tree(
        &self,
        checkout_id: &str,
        expected_checksum: &str,
    ) -> Result<DiffSnapshot, EngineError> {
        let entry = lock(&self.inner.entries)
            .get(checkout_id)
            .cloned()
            .ok_or_else(|| EngineError::Other("checkout is no longer available".into()))?;
        let _guard = entry.discard_lock.lock().await;
        let result =
            discard_working_tree(&self.inner.repos, &entry.identity.root, expected_checksum).await;

        // Publish immediately instead of waiting for the watcher debounce.
        // The watcher kick remains useful if an external writer races this
        // operation after the final capture.
        sync_entry(&self.inner, &entry).await;
        let _ = entry.kick_tx.send(());
        result
    }
}

// ---------------------------------------------------------------------------
// Reconcile: chats ⇄ checkout entries
// ---------------------------------------------------------------------------

/// Resolve `cwd` to its canonical checkout identity, preferring the memo.
///
/// [`Repos::checkout_identity`] spawns `git rev-parse` twice, and reconcile
/// runs on *every* workspace chat row change — including the `branch` and
/// `checkoutId` writes [`sync_entry`] itself makes. Resolving fresh on every
/// pass had two failure modes that combined into a runaway loop on a checkout
/// whose captures cost seconds of CPU:
///
/// 1. every publish fanned out into a storm of `git` spawns (fd pressure);
/// 2. one transient spawn failure (e.g. EMFILE) made the chat ungroupable, so
///    reconcile tore its entry down and re-added it on the next pass — and
///    every re-add kicks a full capture, whose row writes trigger the next
///    reconcile. Back-to-back `git diff` forever, on an idle tree.
///
/// So: chat-watch reconciles reuse the memo (no git at all), the repair tick
/// revalidates (`fresh`), and a failed fresh resolve keeps the memo unless the
/// directory is actually gone.
async fn resolve_identity(
    inner: &Arc<DiffSyncInner>,
    cwd: &str,
    fresh: bool,
) -> Option<CheckoutIdentity> {
    if !fresh && let Some(identity) = lock(&inner.identities).get(cwd).cloned() {
        return Some(identity);
    }
    match inner.repos.checkout_identity(Path::new(cwd)).await {
        Ok(identity) => {
            lock(&inner.identities).insert(cwd.to_string(), identity.clone());
            Some(identity)
        }
        Err(err) => {
            if !Path::new(cwd).exists() {
                lock(&inner.identities).remove(cwd);
                tracing::debug!(cwd = %cwd, error = %err, "diff-sync: checkout gone");
                return None;
            }
            let cached = lock(&inner.identities).get(cwd).cloned();
            match &cached {
                Some(_) => tracing::debug!(cwd = %cwd, error = %err,
                    "diff-sync: identity resolve failed; keeping memoized identity"),
                None => tracing::debug!(cwd = %cwd, error = %err, "diff-sync: not a checkout"),
            }
            cached
        }
    }
}

async fn reconcile(inner: &Arc<DiffSyncInner>, chats: Vec<Chat>, fresh: bool) {
    // One pass at a time — see `reconcile_gate`.
    let _gate = inner.reconcile_gate.lock().await;
    // Group this device's cwd-bearing chats by canonical checkout identity.
    let mut groups: HashMap<String, (CheckoutIdentity, Vec<Chat>)> = HashMap::new();
    // Dedupe resolution within this pass — many chats share one checkout.
    let mut resolved: HashMap<String, Option<CheckoutIdentity>> = HashMap::new();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(cwd) = chat.cwd.clone() else {
            continue;
        };
        let identity = match resolved.get(&cwd) {
            Some(identity) => identity.clone(),
            None => {
                let identity = resolve_identity(inner, &cwd, fresh).await;
                resolved.insert(cwd.clone(), identity.clone());
                identity
            }
        };
        let Some(identity) = identity else {
            continue;
        };
        // Stamp the row's checkoutId so every device groups this chat correctly.
        if chat.checkout_id.as_deref() != Some(identity.id.as_str())
            && let Err(err) = inner.workspace.set_chat_checkout(&chat.id, &identity.id)
        {
            tracing::debug!(chat = %chat.id, error = %err, "diff-sync: checkoutId write failed");
        }
        groups
            .entry(identity.id.clone())
            .or_insert_with(|| (identity, Vec::new()))
            .1
            .push(chat);
    }

    // Close entries whose checkout has had no chats for a full grace period;
    // drop their published diff. A single pass that misses a checkout only
    // *marks* it — teardown is expensive to undo (re-add kicks a capture), so
    // absence must be sustained before we act on it.
    let removed: Vec<String> = {
        let now = std::time::Instant::now();
        let mut entries = lock(&inner.entries);
        let mut removed = Vec::new();
        for (id, entry) in entries.iter() {
            if groups.contains_key(id) {
                *lock(&entry.orphaned_since) = None;
                continue;
            }
            let mut orphaned = lock(&entry.orphaned_since);
            match *orphaned {
                None => *orphaned = Some(now),
                Some(since) if now.duration_since(since) >= inner.orphan_grace => {
                    removed.push(id.clone());
                }
                Some(_) => {}
            }
        }
        for id in &removed {
            entries.remove(id); // dropping the entry drops watchers + ends its task
        }
        removed
    };
    if !removed.is_empty() {
        publish_watch(inner);
    }

    // Update surviving entries; add new ones (initial sync kicked on add).
    for (checkout_id, (identity, chats)) in groups {
        let existing = lock(&inner.entries).get(&checkout_id).cloned();
        match existing {
            Some(entry) => {
                let has_new = {
                    let mut held = lock(&entry.chats);
                    let previous: HashSet<String> = held.iter().map(|c| c.id.clone()).collect();
                    let has_new = chats.iter().any(|c| !previous.contains(&c.id));
                    *held = chats;
                    has_new
                };
                if has_new {
                    let _ = entry.kick_tx.send(()); // new chat needs a sidecar now
                }
            }
            None => add_entry(inner, identity, chats),
        }
    }
}

/// True if `root`'s directory tree exceeds [`MAX_WATCH_DIRS`] — the signal that
/// a live recursive watch would cost more than it's worth. Bounded BFS: stops
/// the moment the budget is blown (never walks a whole node_modules), skips
/// symlinks (a symlinked dep cycle must not send this into a spin), and treats
/// unreadable dirs as leaves. `.git` internal churn is real diff signal, so it
/// counts toward the budget rather than being skipped.
fn exceeds_watch_budget(root: &Path) -> bool {
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut seen = 0usize;
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type()` on the dirent does NOT follow symlinks — a symlinked
            // directory reports as a symlink and is skipped, so cyclic deps
            // (pnpm/npm) can't blow up the walk.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                seen += 1;
                if seen > MAX_WATCH_DIRS {
                    return true;
                }
                queue.push_back(entry.path());
            }
        }
    }
    false
}

/// Which paths a checkout's entry live-watches.
///
/// A recursive `notify` watch installs one OS watch per subdirectory and has
/// no way to prune subtrees. On a checkout carrying big dependency trees
/// (node_modules, target/, vendored deps) that is tens of thousands of
/// watches: the watcher thread pegs a core just maintaining them — even with
/// the tree completely idle — which starved a real device's whole async
/// runtime (presence heartbeats and IPC stalled; it showed permanently
/// offline). So the worktree root is only watched when it fits
/// [`MAX_WATCH_DIRS`].
///
/// An over-budget root still gets its GIT DIR watched (a bounded tree —
/// objects fanout + refs): commits, index moves, and branch switches then
/// refresh the diff instantly, and only raw working-tree edits wait for the
/// repair tick. Without this, a commit right after an edit left the pane
/// showing the pre-commit diff for up to two minutes (user report — the
/// dev checkout's target/ alone blows the budget). Linked worktrees keep
/// their git dir outside the root, so it rides along whenever the root's
/// watch doesn't already cover it.
fn watch_targets(identity: &CheckoutIdentity) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    let root_fits = !exceeds_watch_budget(&identity.root);
    if root_fits {
        targets.push(identity.root.clone());
    } else {
        tracing::info!(path = %identity.root.display(),
            "diff-sync: tree too large to watch live; watching the git dir, edits ride the repair tick");
    }
    let git_covered = root_fits && identity.git_dir.starts_with(&identity.root);
    if !git_covered && !exceeds_watch_budget(&identity.git_dir) {
        targets.push(identity.git_dir.clone());
    }
    targets
}

fn add_entry(inner: &Arc<DiffSyncInner>, identity: CheckoutIdentity, chats: Vec<Chat>) {
    let (kick_tx, kick_rx) = mpsc::unbounded_channel();
    let entry = Arc::new(CheckoutEntry {
        identity,
        chats: Mutex::new(chats),
        checksum: Mutex::new(None),
        orphaned_since: Mutex::new(None),
        kick_tx: kick_tx.clone(),
        discard_lock: tokio::sync::Mutex::new(()),
        watchers: Mutex::new(Vec::new()),
    });
    lock(&inner.entries).insert(entry.identity.id.clone(), entry.clone());
    tokio::spawn(entry_task(
        Arc::downgrade(inner),
        Arc::downgrade(&entry),
        kick_rx,
        inner.cancel.clone(),
    ));
    let _ = kick_tx.send(()); // initial snapshot — must not wait for watchers

    // Watcher setup is genuinely blocking: the budget walk reads up to
    // MAX_WATCH_DIRS directory entries and FSEvents stream registration stalls
    // for seconds when fseventsd is contended. Doing it inline starved the whole
    // runtime (workspace watches, presence) whenever entries were (re)built, so
    // it runs on the blocking pool and attaches to the entry when ready. Events
    // occurring before attachment are covered by the initial sync; one extra
    // kick after attachment closes the capture→attach gap.
    let weak = Arc::downgrade(&entry);
    tokio::task::spawn_blocking(move || {
        let Some(entry) = weak.upgrade() else {
            return; // entry removed before watchers were ready
        };
        let watchers = build_watchers(&entry.identity, &kick_tx);
        *lock(&entry.watchers) = watchers;
        let _ = kick_tx.send(());
    });
}

/// Recursive watchers on the worktree root (budget permitting) and the git
/// dir — HEAD/index churn and file edits both land here. Failures are fine:
/// the initial + repair sync still keep the snapshot correct. Blocking — call
/// from the blocking pool.
fn build_watchers(
    identity: &CheckoutIdentity,
    kick_tx: &mpsc::UnboundedSender<()>,
) -> Vec<notify::RecommendedWatcher> {
    let mut watchers = Vec::new();
    for target in watch_targets(identity) {
        let tx = kick_tx.clone();
        let watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if event.is_ok() {
                    let _ = tx.send(());
                }
            });
        match watcher {
            Ok(mut watcher) => {
                use notify::Watcher as _;
                match watcher.watch(&target, notify::RecursiveMode::Recursive) {
                    Ok(()) => watchers.push(watcher),
                    Err(err) => {
                        tracing::debug!(path = %target.display(), error = %err, "diff-sync: watch failed")
                    }
                }
            }
            Err(err) => tracing::debug!(error = %err, "diff-sync: watcher create failed"),
        }
    }
    watchers
}

/// Per-checkout task: trailing-debounce fs kicks, then compute + publish. Runs
/// syncs sequentially — kicks during a sync accumulate and trigger another pass.
async fn entry_task(
    inner: Weak<DiffSyncInner>,
    entry: Weak<CheckoutEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
    cancel: CancellationToken,
) {
    while kick_rx.recv().await.is_some() {
        // Trailing debounce: wait for the burst to settle.
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
        // The upgraded Arc would let a sync outlive shutdown — race the token
        // so an in-flight sidecar POST is dropped, not completed.
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = sync_entry(&inner, &entry) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot + publish
// ---------------------------------------------------------------------------

async fn sync_entry(inner: &Arc<DiffSyncInner>, entry: &Arc<CheckoutEntry>) {
    let snapshot = match capture_diff(&inner.repos, &entry.identity.root).await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::debug!(checkout = %entry.identity.root.display(), error = %err,
                "diff-sync: capture failed");
            return;
        }
    };

    if lock(&entry.checksum).as_deref() == Some(snapshot.checksum.as_str()) {
        return; // unchanged — publish nothing
    }
    *lock(&entry.checksum) = Some(snapshot.checksum.clone());

    let diff = CheckoutDiff {
        checkout_id: entry.identity.id.clone(),
        device_id: inner.device_id.clone(),
        cwd: entry.identity.root.to_string_lossy().to_string(),
        patch: snapshot.patch.clone(),
        files: snapshot.files.clone(),
        additions: snapshot.additions,
        deletions: snapshot.deletions,
        truncated: snapshot.truncated,
        checksum: snapshot.checksum.clone(),
        updated_at: chrono::Utc::now(),
    };
    {
        let entries = lock(&inner.entries);
        if !entries.contains_key(&entry.identity.id) {
            return; // closed while computing
        }
    }
    publish_watch_with(inner, Some(diff));

    // Latest-only sidecar to every syncing chat's session DO slot.
    let chats = lock(&entry.chats).clone();
    if let Some(edge) = &inner.edge {
        for chat in &chats {
            let sidecar = DiffSidecar {
                chat_id: chat.id.clone(),
                device_id: inner.device_id.clone(),
                checkout_path: entry.identity.root.to_string_lossy().to_string(),
                branch: Some(snapshot.branch.clone()),
                head_sha: snapshot.head_sha.clone(),
                patch: snapshot.patch.clone(),
                files: snapshot.files.clone(),
                additions: snapshot.additions,
                deletions: snapshot.deletions,
                truncated: snapshot.truncated,
                published_at: chrono::Utc::now().timestamp_millis(),
            };
            let url = format!("{}/diff/{}", edge.url.trim_end_matches('/'), chat.id);
            // Fresh bearer per request — never the boot-time snapshot.
            let Some(bearer) = edge.bearer().await else {
                tracing::debug!(chat = %chat.id, "diff-sync: sidecar skipped (signed out)");
                continue;
            };
            let result = inner
                .http
                .post(&url)
                .bearer_auth(&bearer)
                .json(&sidecar)
                .send()
                .await;
            match result {
                Ok(response) if !response.status().is_success() => {
                    tracing::debug!(chat = %chat.id, status = %response.status(),
                        "diff-sync: sidecar publish rejected");
                }
                Err(err) => {
                    tracing::debug!(chat = %chat.id, error = %err, "diff-sync: sidecar publish failed");
                }
                Ok(_) => {}
            }
        }
    }
}

/// Re-emit the watch channel from the current entries' cached diffs, replacing (or
/// inserting) `updated`.
fn publish_watch_with(inner: &Arc<DiffSyncInner>, updated: Option<CheckoutDiff>) {
    let live: HashSet<String> = lock(&inner.entries).keys().cloned().collect();
    inner.diffs_tx.send_modify(|diffs| {
        diffs.retain(|d| live.contains(&d.checkout_id));
        if let Some(updated) = updated {
            match diffs
                .iter_mut()
                .find(|d| d.checkout_id == updated.checkout_id)
            {
                Some(slot) => *slot = updated,
                None => diffs.push(updated),
            }
        }
        diffs.sort_by(|a, b| a.checkout_id.cmp(&b.checkout_id));
    });
}

fn publish_watch(inner: &Arc<DiffSyncInner>) {
    publish_watch_with(inner, None);
}

/// Chat-watch follower + repair tick. Holds only weak handles so dropping the
/// service tears the loop down; the token ends it eagerly on shutdown.
async fn diff_sync_task(
    inner: Weak<DiffSyncInner>,
    mut chats_rx: watch::Receiver<Vec<Chat>>,
    cancel: CancellationToken,
) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            changed = chats_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow_and_update().clone();
                // Memoized identities only: chat rows change constantly (the
                // sync itself writes them) and must never fan out into git.
                reconcile(&inner, chats, false).await;
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow().clone();
                // Fresh: revalidate every memoized identity against git.
                reconcile(&inner, chats, true).await;
                for entry in lock(&inner.entries).values() {
                    let _ = entry.kick_tx.send(());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Diff capture (exposed for tests)
// ---------------------------------------------------------------------------

struct Capture {
    stdout: Vec<u8>,
    truncated: bool,
}

/// Run git capturing stdout under a hard byte ceiling — the child is killed once
/// the cap is hit, so an arbitrarily large repository diff never buffers fully.
async fn capture_git(cwd: &Path, args: &[&str], max_bytes: usize) -> Result<Capture, EngineError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| EngineError::Other("git stdout unavailable".into()))?;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = stdout
            .read(&mut buf)
            .await
            .map_err(|e| EngineError::Other(format!("git read failed: {e}")))?;
        if n == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(out.len());
        if n > remaining {
            out.extend_from_slice(&buf[..remaining]);
            truncated = true;
            let _ = child.start_kill();
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| EngineError::Other(format!("git wait failed: {e}")))?;
    if !output.status.success() && !truncated {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(EngineError::Other(if message.is_empty() {
            format!("git exited {}", output.status)
        } else {
            format!("git: {message}")
        }));
    }
    Ok(Capture {
        stdout: out,
        truncated,
    })
}

fn split_z(value: &[u8]) -> Vec<String> {
    value
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect()
}

/// Git paths are arbitrary bytes on Unix, while the diff protocol exposes
/// paths as UTF-8 strings. Treat an unrepresentable path as a partial snapshot
/// so destructive actions cannot operate on a file the UI could not faithfully
/// display or checksum.
fn has_non_utf8_status_path(value: &[u8]) -> bool {
    let records: Vec<&[u8]> = value
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();
    let mut i = 0usize;
    while i < records.len() {
        let record = records[i];
        i += 1;
        if record.len() < 3 || record[2] != b' ' {
            continue;
        }
        if std::str::from_utf8(&record[3..]).is_err() {
            return true;
        }
        if (record.starts_with(b"R") || record.starts_with(b"C")) && i < records.len() {
            if std::str::from_utf8(records[i]).is_err() {
                return true;
            }
            i += 1;
        }
    }
    false
}

fn parse_name_status(value: &[u8]) -> Vec<DiffFileSummary> {
    let fields = split_z(value);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < fields.len() {
        let raw = fields[i].clone();
        i += 1;
        let code = raw.chars().next().unwrap_or('M');
        let Some(first) = fields.get(i).cloned() else {
            break;
        };
        i += 1;
        let renamed = code == 'R' || code == 'C';
        let second = if renamed {
            let s = fields.get(i).cloned();
            i += 1;
            s
        } else {
            None
        };
        let status = match code {
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'U' => "unmerged",
            _ => "modified",
        };
        out.push(DiffFileSummary {
            path: second.clone().unwrap_or_else(|| first.clone()),
            old_path: second.is_some().then_some(first),
            status: status.to_string(),
            additions: 0,
            deletions: 0,
            binary: false,
        });
    }
    out
}

fn apply_numstat(files: &mut [DiffFileSummary], value: &[u8]) {
    // With -z, a rename record is `adds<TAB>dels<TAB><NUL>old<NUL>new<NUL>`.
    let records: Vec<String> = value
        .split(|b| *b == 0)
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    let mut i = 0usize;
    while i < records.len() {
        let record = &records[i];
        if record.is_empty() {
            i += 1;
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let adds = parts.next().unwrap_or_default().to_string();
        let dels = parts.next().unwrap_or_default().to_string();
        let inline_path = parts.next().unwrap_or_default().to_string();
        let path = if inline_path.is_empty() {
            // Rename: the next two records are old, new.
            let new_path = records.get(i + 2).cloned().unwrap_or_default();
            i += 2;
            new_path
        } else {
            inline_path
        };
        i += 1;
        if let Some(file) = files.iter_mut().find(|f| f.path == path) {
            file.additions = adds.parse().unwrap_or(0);
            file.deletions = dels.parse().unwrap_or(0);
            file.binary = adds == "-" || dels == "-";
        }
    }
}

fn quote_patch_path(path: &str) -> String {
    if path
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        serde_json::to_string(path).unwrap_or_else(|_| format!("\"{path}\""))
    } else {
        path.to_string()
    }
}

/// Synthesize a new-file hunk for an untracked file (git diff never shows them).
fn untracked_patch(path: &str, content: &str) -> String {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let body: String = lines
        .iter()
        .map(|line| format!("+{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let a = quote_patch_path(&format!("a/{path}"));
    let b = quote_patch_path(&format!("b/{path}"));
    format!(
        "diff --git {a} {b}\nnew file mode 100644\n--- /dev/null\n+++ {b}\n@@ -0,0 +1,{} @@\n{body}\n",
        lines.len()
    )
}

fn validate_diff_path(path: &str) -> Result<&Path, EngineError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(EngineError::Other("diff path escapes checkout".into()));
    }
    Ok(path)
}

fn decode_diff_source(
    bytes: Vec<u8>,
) -> Result<(Option<String>, Option<String>, bool), EngineError> {
    if bytes.contains(&0) {
        return Ok((None, None, true));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Ok((None, None, true)),
    };
    let hash = crate::repos::hex(&Sha256::digest(text.as_bytes()));
    Ok((Some(text), Some(hash), false))
}

async fn read_worktree_source(root: &Path, path: &Path) -> Result<Capture, EngineError> {
    let canonical_root = tokio::fs::canonicalize(root)
        .await
        .map_err(|error| EngineError::Other(format!("canonical checkout: {error}")))?;
    let full = root.join(path);
    let metadata = tokio::fs::symlink_metadata(&full)
        .await
        .map_err(|error| EngineError::Other(format!("read diff file metadata: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EngineError::Other(
            "diff source is not a regular checkout file".into(),
        ));
    }
    let canonical = tokio::fs::canonicalize(&full)
        .await
        .map_err(|error| EngineError::Other(format!("canonical diff file: {error}")))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(EngineError::Other("diff source escapes checkout".into()));
    }
    if metadata.len() > MAX_DIFF_SOURCE_BYTES as u64 {
        return Ok(Capture {
            stdout: Vec::new(),
            truncated: true,
        });
    }
    let stdout = tokio::fs::read(&canonical)
        .await
        .map_err(|error| EngineError::Other(format!("read diff file: {error}")))?;
    Ok(Capture {
        stdout,
        truncated: false,
    })
}

async fn read_git_source(root: &Path, revision: &str, path: &Path) -> Result<Capture, EngineError> {
    let spec = format!("{revision}:{}", path.to_string_lossy());
    capture_git(root, &["cat-file", "blob", &spec], MAX_DIFF_SOURCE_BYTES).await
}

/// Read the exact old/new documents for one file in a previously captured diff.
/// Paths must come from that snapshot's file summary; callers still recheck the
/// snapshot checksum after this read to close the filesystem race.
pub async fn read_diff_file_text(
    root: &Path,
    base: &str,
    file: &DiffFileSummary,
) -> Result<DiffFileTextPair, EngineError> {
    read_diff_file_text_at(root, base, None, file).await
}

/// Read the exact old/new documents for one file in a diff between `base` and
/// an optional committed target. Without a target, the new source is the live
/// working tree; with one, both sources are immutable Git blobs.
pub(crate) async fn read_diff_file_text_at(
    root: &Path,
    base: &str,
    target: Option<&str>,
    file: &DiffFileSummary,
) -> Result<DiffFileTextPair, EngineError> {
    let new_path = validate_diff_path(&file.path)?;
    let old_path = validate_diff_path(file.old_path.as_deref().unwrap_or(&file.path))?;

    let old = if file.status == "added" {
        None
    } else {
        Some(read_git_source(root, base, old_path).await?)
    };
    let new = if file.status == "deleted" {
        None
    } else if let Some(target) = target {
        Some(read_git_source(root, target, new_path).await?)
    } else {
        Some(read_worktree_source(root, new_path).await?)
    };
    let truncated = old.as_ref().is_some_and(|source| source.truncated)
        || new.as_ref().is_some_and(|source| source.truncated);
    if truncated {
        return Ok(DiffFileTextPair {
            old_text: None,
            new_text: None,
            old_content_hash: None,
            new_content_hash: None,
            binary: false,
            truncated: true,
        });
    }
    let (old_text, old_content_hash, old_binary) = match old {
        Some(source) => decode_diff_source(source.stdout)?,
        None => (None, None, false),
    };
    let (new_text, new_content_hash, new_binary) = match new {
        Some(source) => decode_diff_source(source.stdout)?,
        None => (None, None, false),
    };
    let binary = old_binary || new_binary || file.binary;
    Ok(DiffFileTextPair {
        old_text: (!binary).then_some(old_text).flatten(),
        new_text: (!binary).then_some(new_text).flatten(),
        old_content_hash: (!binary).then_some(old_content_hash).flatten(),
        new_content_hash: (!binary).then_some(new_content_hash).flatten(),
        binary,
        truncated: false,
    })
}

/// Resolve the parent used as a commit diff's old side. Root commits compare
/// against Git's canonical empty tree.
pub(crate) async fn commit_diff_base(root: &Path, sha: &str) -> String {
    let parent_spec = format!("{sha}^");
    let parent = capture_git(root, &["rev-parse", "--verify", &parent_spec], 256)
        .await
        .map(|capture| String::from_utf8_lossy(&capture.stdout).trim().to_string())
        .unwrap_or_default();
    if parent.is_empty() {
        EMPTY_TREE_SHA.to_string()
    } else {
        parent
    }
}

pub async fn working_diff_base(root: &Path) -> Result<String, EngineError> {
    let head = capture_git(root, &["rev-parse", "--verify", "HEAD"], 256)
        .await
        .map(|capture| String::from_utf8_lossy(&capture.stdout).trim().to_string())
        .unwrap_or_default();
    Ok(if head.is_empty() {
        EMPTY_TREE_SHA.into()
    } else {
        head
    })
}

/// One bounded atomic snapshot: tracked diff vs HEAD (or the empty tree) with
/// renames, plus untracked files (via `git status --porcelain`, index untouched)
/// as synthesized new-file hunks. 3MiB patch cap with a `truncated` flag; sha256
/// checksum over branch ‖ head ‖ patch ‖ files ‖ truncated.
pub async fn capture_diff(repos: &Repos, root: &Path) -> Result<DiffSnapshot, EngineError> {
    capture_diff_against(repos, root, None).await
}

/// [`capture_diff`] with the diff base overridable: `None` keeps the
/// working-tree behavior (vs HEAD / the empty tree); `Some(committish)` diffs
/// the working tree against that base instead ("Branch changes" passes the
/// merge-base with the comparison ref). Untracked files synthesize as new
/// either way — they are new relative to any committed base.
pub async fn capture_diff_against(
    repos: &Repos,
    root: &Path,
    base_override: Option<&str>,
) -> Result<DiffSnapshot, EngineError> {
    let head = capture_git(root, &["rev-parse", "--verify", "HEAD"], 256)
        .await
        .map(|c| String::from_utf8_lossy(&c.stdout).trim().to_string())
        .unwrap_or_default();
    let base: &str = match base_override {
        Some(base) => base,
        None if head.is_empty() => EMPTY_TREE_SHA,
        None => &head,
    };
    let branch = repos
        .current_branch(root)
        .await
        .unwrap_or_else(|_| "HEAD".into());

    let names = capture_git(
        root,
        &["diff", "--name-status", "-z", "--find-renames", base, "--"],
        2 * 1024 * 1024,
    )
    .await?;
    let nums = capture_git(
        root,
        &["diff", "--numstat", "-z", "--find-renames", base, "--"],
        2 * 1024 * 1024,
    )
    .await?;
    let tracked = capture_git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            base,
            "--",
        ],
        MAX_PATCH_BYTES,
    )
    .await?;
    // Untracked listing via porcelain status; `--no-optional-locks` keeps this
    // read-only (a status-triggered index refresh would re-kick our own watcher).
    let status = capture_git(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
        ],
        2 * 1024 * 1024,
    )
    .await?;

    let mut files = parse_name_status(&names.stdout);
    apply_numstat(&mut files, &nums.stdout);
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();
    let mut truncated = tracked.truncated || names.truncated || nums.truncated || status.truncated;
    if has_non_utf8_status_path(&status.stdout) {
        truncated = true;
    }

    if tracked.truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Zeron diff truncated\n");
    }

    // `?? path` records; rename records (`R  new\0old`) consume their extra field.
    let mut untracked: Vec<String> = Vec::new();
    let records = split_z(&status.stdout);
    let mut i = 0usize;
    while i < records.len() {
        let record = &records[i];
        i += 1;
        if record.len() < 3 {
            continue;
        }
        let (code, path) = record.split_at(2);
        if code.starts_with('R') || code.starts_with('C') {
            i += 1; // skip the origin-path field
        }
        if code == "??" {
            untracked.push(path.trim_start().to_string());
        }
    }
    untracked.sort();

    for path in untracked {
        let full = root.join(&path);
        let binary;
        let mut additions = 0u32;
        let metadata = tokio::fs::metadata(&full).await.ok();
        let size = metadata
            .as_ref()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if metadata.as_ref().is_some_and(|metadata| metadata.is_dir()) {
            // Git keeps nested repositories atomic even with
            // `--untracked-files=all`. Surface the directory in the snapshot,
            // but never read through or clean it recursively.
            binary = true;
        } else if size > MAX_PATCH_BYTES as u64 {
            binary = true;
            truncated = true;
        } else {
            match tokio::fs::read(&full).await {
                Ok(bytes) => {
                    binary = bytes.contains(&0);
                    if !binary {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        additions = if text.is_empty() {
                            0
                        } else {
                            (text.split('\n').count() - usize::from(text.ends_with('\n'))) as u32
                        };
                        let addition = untracked_patch(&path, &text);
                        if patch.len() + addition.len() <= MAX_PATCH_BYTES {
                            if !patch.is_empty() && !patch.ends_with('\n') {
                                patch.push('\n');
                            }
                            patch.push_str(&addition);
                        } else {
                            truncated = true;
                        }
                    }
                }
                Err(_) => continue, // vanished between status and read
            }
        }
        files.push(DiffFileSummary {
            path,
            old_path: None,
            status: "added".to_string(),
            additions,
            deletions: 0,
            binary,
        });
    }

    let additions: u32 = files.iter().map(|f| f.additions).sum();
    let deletions: u32 = files.iter().map(|f| f.deletions).sum();
    let files_json = serde_json::to_string(&files)
        .map_err(|e| EngineError::Other(format!("diff files serialize: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(branch.as_bytes());
    hasher.update([0u8]);
    hasher.update(head.as_bytes());
    hasher.update([0u8]);
    hasher.update(patch.as_bytes());
    hasher.update([0u8]);
    hasher.update(files_json.as_bytes());
    hasher.update(if truncated { b"1" } else { b"0" });
    let checksum = crate::repos::hex(&hasher.finalize());

    Ok(DiffSnapshot {
        branch,
        head_sha: (!head.is_empty()).then_some(head),
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum,
    })
}

fn status_paths(status: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let records: Vec<&[u8]> = status
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();
    let mut tracked = Vec::new();
    let mut untracked = Vec::new();
    let mut i = 0usize;
    while i < records.len() {
        let record = records[i];
        i += 1;
        if record.len() < 4 || record[2] != b' ' {
            continue;
        }
        let code = &record[..2];
        let path = record[3..].to_vec();
        if code == b"??" {
            untracked.push(path);
            continue;
        }
        if code == b"!!" {
            continue;
        }
        tracked.push(path);
        if (code.contains(&b'R') || code.contains(&b'C')) && i < records.len() {
            tracked.push(records[i].to_vec());
            i += 1;
        }
    }
    tracked.sort();
    tracked.dedup();
    untracked.sort();
    untracked.dedup();
    (tracked, untracked)
}

#[cfg(unix)]
fn path_argument(path: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(path.to_vec())
}

#[cfg(not(unix))]
fn path_argument(path: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(path).into_owned())
}

async fn run_git_for_paths(
    root: &Path,
    fixed_args: &[OsString],
    paths: &[Vec<u8>],
) -> Result<(), EngineError> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .arg("--literal-pathspecs")
        .args(fixed_args)
        .arg("--");
    for path in paths {
        command.arg(path_argument(path));
    }
    command.stdin(std::process::Stdio::null());
    let output = command
        .output()
        .await
        .map_err(|error| EngineError::Other(format!("git spawn failed: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    Err(EngineError::Other(if message.is_empty() {
        format!("git discard failed ({})", output.status)
    } else {
        format!("git: {message}")
    }))
}

/// Restore staged and unstaged tracked paths to the snapshot's HEAD, then ask
/// Git to remove only the exact untracked paths it reported. Ignored files are
/// excluded by porcelain status and `git clean` intentionally omits `-x`;
/// nested repositories and submodule contents are never recursively cleaned.
pub async fn discard_working_tree(
    repos: &Repos,
    root: &Path,
    expected_checksum: &str,
) -> Result<DiffSnapshot, EngineError> {
    let snapshot = capture_diff(repos, root).await?;
    let head = snapshot.head_sha.as_deref().ok_or_else(|| {
        EngineError::Other("cannot discard changes before the first commit".into())
    })?;
    if snapshot.truncated {
        return Err(EngineError::Other(
            "cannot safely discard a partial diff snapshot".into(),
        ));
    }
    if snapshot.checksum != expected_checksum {
        return Err(EngineError::Other(
            "working tree changed since the confirmation was opened".into(),
        ));
    }

    let status = capture_git(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    if status.truncated {
        return Err(EngineError::Other(
            "cannot safely discard a truncated working-tree status".into(),
        ));
    }
    let (tracked, untracked) = status_paths(&status.stdout);
    if untracked.iter().any(|path| {
        std::fs::symlink_metadata(root.join(PathBuf::from(path_argument(path))))
            .is_ok_and(|metadata| metadata.is_dir())
    }) {
        return Err(EngineError::Other(
            "cannot discard an untracked nested repository safely".into(),
        ));
    }

    let restore_args = [
        OsString::from("restore"),
        OsString::from(format!("--source={head}")),
        OsString::from("--staged"),
        OsString::from("--worktree"),
    ];
    run_git_for_paths(root, &restore_args, &tracked).await?;
    let clean_args = [OsString::from("clean"), OsString::from("-fd")];
    run_git_for_paths(root, &clean_args, &untracked).await?;
    // `--untracked-files=all` gives exact files, so `git clean` can leave
    // their now-empty parent directories behind. Remove only empty ancestors;
    // ignored files or any concurrent writer make `remove_dir` stop safely.
    for path in &untracked {
        let full = root.join(PathBuf::from(path_argument(path)));
        let mut parent = full.parent();
        while let Some(directory) = parent {
            if directory == root || std::fs::remove_dir(directory).is_err() {
                break;
            }
            parent = directory.parent();
        }
    }

    let final_snapshot = capture_diff(repos, root).await?;
    if !final_snapshot.files.is_empty() || !final_snapshot.patch.trim().is_empty() {
        return Err(EngineError::Other(
            "some changes could not be discarded safely".into(),
        ));
    }
    Ok(final_snapshot)
}

/// Snapshot of one COMMIT's changes: first-parent (or the empty tree for a
/// root commit) diffed against the commit itself — the History pane's
/// per-commit tab. Commit-to-commit only: no working tree, no untracked
/// synthesis.
pub async fn capture_commit_diff(
    repos: &Repos,
    root: &Path,
    sha: &str,
) -> Result<DiffSnapshot, EngineError> {
    let base = commit_diff_base(root, sha).await;
    let branch = repos
        .current_branch(root)
        .await
        .unwrap_or_else(|_| "HEAD".into());
    let names = capture_git(
        root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            &base,
            sha,
            "--",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let nums = capture_git(
        root,
        &[
            "diff",
            "--numstat",
            "-z",
            "--find-renames",
            &base,
            sha,
            "--",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let tracked = capture_git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            &base,
            sha,
            "--",
        ],
        MAX_PATCH_BYTES,
    )
    .await?;
    let mut files = parse_name_status(&names.stdout);
    apply_numstat(&mut files, &nums.stdout);
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();
    let truncated = tracked.truncated || names.truncated || nums.truncated;
    if tracked.truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Comet diff truncated\n");
    }
    let additions: u32 = files.iter().map(|f| f.additions).sum();
    let deletions: u32 = files.iter().map(|f| f.deletions).sum();
    let files_json = serde_json::to_string(&files)
        .map_err(|e| EngineError::Other(format!("diff files serialize: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(branch.as_bytes());
    hasher.update([0u8]);
    hasher.update(sha.as_bytes());
    hasher.update([0u8]);
    hasher.update(patch.as_bytes());
    hasher.update([0u8]);
    hasher.update(files_json.as_bytes());
    hasher.update(if truncated { b"1" } else { b"0" });
    let checksum = crate::repos::hex(&hasher.finalize());
    Ok(DiffSnapshot {
        branch,
        head_sha: Some(sha.to_string()),
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum,
    })
}

/// `git merge-base <base_ref> HEAD` — the diff base for "Branch changes".
/// Errors when the ref is unknown or the histories are unrelated.
pub async fn merge_base(root: &Path, base_ref: &str) -> Result<String, EngineError> {
    let capture = capture_git(root, &["merge-base", base_ref, "HEAD"], 256).await?;
    let sha = String::from_utf8_lossy(&capture.stdout).trim().to_string();
    if sha.is_empty() {
        return Err(EngineError::Other(format!("no merge base with {base_ref}")));
    }
    Ok(sha)
}

/// Write the checkout's current tracked + untracked (unignored) tree into the
/// object db via a throwaway index: `git add -A` under `GIT_INDEX_FILE`, then
/// `git write-tree`. The real index is never touched. Costs one full hash pass
/// over the working tree (no stat cache in a fresh index) — run once per turn
/// dispatch, that is the same cost class as the untracked-file reads the watch
/// capture already does.
pub async fn snapshot_tree(root: &Path) -> Result<String, EngineError> {
    let index = std::env::temp_dir().join(format!(
        "zeron-turn-index-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    ));
    let run = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("-C").arg(root).args(args);
        cmd.env("GIT_INDEX_FILE", &index);
        cmd.stdin(std::process::Stdio::null());
        cmd.output()
    };
    let added = run(&["add", "-A", "--ignore-errors", "."])
        .await
        .map_err(|e| EngineError::Other(format!("git add failed: {e}")))?;
    if !added.status.success() {
        let _ = tokio::fs::remove_file(&index).await;
        return Err(EngineError::Other(format!(
            "git add: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        )));
    }
    let written = run(&["write-tree"])
        .await
        .map_err(|e| EngineError::Other(format!("git write-tree failed: {e}")));
    let _ = tokio::fs::remove_file(&index).await;
    let written = written?;
    if !written.status.success() {
        return Err(EngineError::Other(format!(
            "git write-tree: {}",
            String::from_utf8_lossy(&written.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&written.stdout).trim().to_string())
}

/// "Latest turn" capture: tree-to-tree diff from the turn-start snapshot to a
/// fresh [`snapshot_tree`] of the current state. Both trees carry untracked
/// (unignored) files, so no synthesis is needed and a file that was already
/// untracked at turn start diffs correctly (the watch capture's synthesis
/// would misreport it as entirely new).
pub async fn capture_turn_diff(
    repos: &Repos,
    root: &Path,
    turn_tree: &str,
) -> Result<DiffSnapshot, EngineError> {
    let current = snapshot_tree(root).await?;
    let head = capture_git(root, &["rev-parse", "--verify", "HEAD"], 256)
        .await
        .map(|c| String::from_utf8_lossy(&c.stdout).trim().to_string())
        .unwrap_or_default();
    let branch = repos
        .current_branch(root)
        .await
        .unwrap_or_else(|_| "HEAD".into());

    let names = capture_git(
        root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            turn_tree,
            &current,
            "--",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let nums = capture_git(
        root,
        &[
            "diff",
            "--numstat",
            "-z",
            "--find-renames",
            turn_tree,
            &current,
            "--",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let tracked = capture_git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            turn_tree,
            &current,
            "--",
        ],
        MAX_PATCH_BYTES,
    )
    .await?;

    let mut files = parse_name_status(&names.stdout);
    apply_numstat(&mut files, &nums.stdout);
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();
    let truncated = tracked.truncated || names.truncated || nums.truncated;
    if tracked.truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Zeron diff truncated\n");
    }

    let additions: u32 = files.iter().map(|f| f.additions).sum();
    let deletions: u32 = files.iter().map(|f| f.deletions).sum();
    let files_json = serde_json::to_string(&files)
        .map_err(|e| EngineError::Other(format!("diff files serialize: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(branch.as_bytes());
    hasher.update([0u8]);
    hasher.update(head.as_bytes());
    hasher.update([0u8]);
    hasher.update(patch.as_bytes());
    hasher.update([0u8]);
    hasher.update(files_json.as_bytes());
    hasher.update(if truncated { b"1" } else { b"0" });
    let checksum = crate::repos::hex(&hasher.finalize());

    Ok(DiffSnapshot {
        branch,
        head_sha: (!head.is_empty()).then_some(head),
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum,
    })
}

#[cfg(test)]
mod watch_budget_tests {
    use super::{
        CheckoutIdentity, MAX_WATCH_DIRS, exceeds_watch_budget, has_non_utf8_status_path,
        watch_targets,
    };

    #[test]
    fn non_utf8_status_paths_are_marked_unsafe_for_destructive_actions() {
        assert!(has_non_utf8_status_path(b"?? invalid-\xff.txt\0"));
        assert!(!has_non_utf8_status_path(b"?? valid.txt\0"));
    }

    #[test]
    fn small_tree_is_watchable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/a/b")).unwrap();
        std::fs::create_dir_all(root.join("src/c")).unwrap();
        std::fs::write(root.join("src/a/f.txt"), "x").unwrap();
        assert!(!exceeds_watch_budget(root));
    }

    #[test]
    fn budget_is_exceeded_and_probe_stays_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // One flat directory of MAX_WATCH_DIRS + 50 subdirs trips the budget;
        // the BFS must stop right after the threshold, not enumerate the rest.
        for i in 0..(MAX_WATCH_DIRS + 50) {
            std::fs::create_dir(root.join(format!("d{i}"))).unwrap();
        }
        assert!(exceeds_watch_budget(root));
    }

    fn identity(root: &std::path::Path, git_dir: &std::path::Path) -> CheckoutIdentity {
        CheckoutIdentity {
            id: "test".into(),
            root: root.to_path_buf(),
            git_dir: git_dir.to_path_buf(),
        }
    }

    #[test]
    fn small_checkout_watches_root_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // The root watch covers the inline .git — no second watcher.
        assert_eq!(
            watch_targets(&identity(root, &root.join(".git"))),
            vec![root.to_path_buf()]
        );
    }

    #[test]
    fn linked_worktree_watches_root_and_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wt");
        let git_dir = tmp.path().join("main/.git/worktrees/wt");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        assert_eq!(
            watch_targets(&identity(&root, &git_dir)),
            vec![root.clone(), git_dir]
        );
    }

    #[test]
    fn over_budget_root_falls_back_to_the_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        // Blow the budget with a flat dependency-tree stand-in.
        for i in 0..(MAX_WATCH_DIRS + 50) {
            std::fs::create_dir(root.join(format!("d{i}"))).unwrap();
        }
        // Commits/index churn must still watch live even though edits can't.
        assert_eq!(
            watch_targets(&identity(root, &root.join(".git"))),
            vec![root.join(".git")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        // A self-referential symlink cycle must not send the walk into a spin.
        std::os::unix::fs::symlink(root.join("real"), root.join("real/inner/loop")).unwrap();
        assert!(!exceeds_watch_budget(root)); // terminates, under budget
    }
}
