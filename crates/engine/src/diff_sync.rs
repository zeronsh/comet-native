//! CheckoutDiffSync — checkout-scoped working-tree diff production (feature-inventory
//! §3.5; port of comet's `checkout-diff-sync.ts` + `git-metadata-sync.ts`).
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
//! - `chat.branch` upkeep: the same fs events cover the checkout's git dir (HEAD),
//!   so each snapshot reconciles mismatched workspace chat rows' `branch` (and
//!   `checkoutId` at reconcile time).
//!
//! Fast recursive `notify` watchers (debounced [`WATCH_DEBOUNCE`]) are backed by a
//! slow 2-minute repair tick because native watchers may coalesce or drop events.
//! Snapshots carry a sha256 checksum; an unchanged checksum publishes nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use comet_proto::{Chat, CheckoutDiff, DiffFileSummary};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::repos::{CheckoutIdentity, Repos};
use crate::workspace_host::WorkspaceHost;

/// Hard cap on the unified patch (plus untracked hunks) — "Partial snapshot".
pub const MAX_PATCH_BYTES: usize = 3 * 1024 * 1024;
/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: re-reconcile + re-sync every checkout.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);
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

struct CheckoutEntry {
    identity: CheckoutIdentity,
    chats: Mutex<Vec<Chat>>,
    /// Last published checksum — unchanged snapshots publish nothing.
    checksum: Mutex<Option<String>>,
    /// Kick channel into the entry's debounce/sync task.
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the recursive fs watchers alive; dropped on entry close.
    _watchers: Vec<notify::RecommendedWatcher>,
}

struct DiffSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
    entries: Mutex<HashMap<String, Arc<CheckoutEntry>>>,
    diffs_tx: watch::Sender<Vec<CheckoutDiff>>,
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
        let (diffs_tx, _) = watch::channel(Vec::new());
        let sync = Self {
            inner: Arc::new(DiffSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                edge,
                http: reqwest::Client::new(),
                entries: Mutex::new(HashMap::new()),
                diffs_tx,
            }),
        };
        tokio::spawn(diff_sync_task(Arc::downgrade(&sync.inner), workspace.watch_chats()));
        sync
    }

    /// `WatchCheckoutDiffs` source: every tracked checkout's latest diff.
    pub fn watch_diffs(&self) -> watch::Receiver<Vec<CheckoutDiff>> {
        self.inner.diffs_tx.subscribe()
    }

    /// Regroup this device's chats by checkout identity, then (re)build watchers.
    /// Public for tests (the background task calls it on every chat change).
    pub async fn reconcile_now(&self) {
        let chats = self.inner.workspace.watch_chats().borrow().clone();
        reconcile(&self.inner, chats).await;
    }

    /// Kick an immediate sync of every tracked checkout (repair-tick path).
    pub fn sync_all(&self) {
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Reconcile: chats ⇄ checkout entries
// ---------------------------------------------------------------------------

async fn reconcile(inner: &Arc<DiffSyncInner>, chats: Vec<Chat>) {
    // Group this device's cwd-bearing chats by canonical checkout identity.
    let mut groups: HashMap<String, (CheckoutIdentity, Vec<Chat>)> = HashMap::new();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(cwd) = chat.cwd.clone() else { continue };
        let identity = match inner.repos.checkout_identity(Path::new(&cwd)).await {
            Ok(identity) => identity,
            Err(err) => {
                tracing::debug!(cwd = %cwd, error = %err, "diff-sync: not a checkout");
                continue;
            }
        };
        // Stamp the row's checkoutId so every device groups this chat correctly.
        if chat.checkout_id.as_deref() != Some(identity.id.as_str()) {
            if let Err(err) = inner.workspace.set_chat_checkout(&chat.id, &identity.id) {
                tracing::debug!(chat = %chat.id, error = %err, "diff-sync: checkoutId write failed");
            }
        }
        groups.entry(identity.id.clone()).or_insert_with(|| (identity, Vec::new())).1.push(chat);
    }

    // Close entries whose checkout no longer has chats; drop their published diff.
    let removed: Vec<String> = {
        let mut entries = lock(&inner.entries);
        let removed: Vec<String> =
            entries.keys().filter(|id| !groups.contains_key(*id)).cloned().collect();
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
                    let previous: HashSet<String> =
                        held.iter().map(|c| c.id.clone()).collect();
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

fn add_entry(inner: &Arc<DiffSyncInner>, identity: CheckoutIdentity, chats: Vec<Chat>) {
    let (kick_tx, kick_rx) = mpsc::unbounded_channel();

    // Recursive watchers on the worktree root and (for linked worktrees) the git
    // dir — HEAD/index churn and file edits both land here. Failures are fine:
    // the initial + repair sync still keep the snapshot correct.
    let mut watchers = Vec::new();
    let mut targets: Vec<&PathBuf> = vec![&identity.root];
    if !identity.git_dir.starts_with(&identity.root) {
        targets.push(&identity.git_dir);
    }
    for target in targets {
        let tx = kick_tx.clone();
        let watcher = notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            if event.is_ok() {
                let _ = tx.send(());
            }
        });
        match watcher {
            Ok(mut watcher) => {
                use notify::Watcher as _;
                match watcher.watch(target, notify::RecursiveMode::Recursive) {
                    Ok(()) => watchers.push(watcher),
                    Err(err) => {
                        tracing::debug!(path = %target.display(), error = %err, "diff-sync: watch failed")
                    }
                }
            }
            Err(err) => tracing::debug!(error = %err, "diff-sync: watcher create failed"),
        }
    }

    let entry = Arc::new(CheckoutEntry {
        identity,
        chats: Mutex::new(chats),
        checksum: Mutex::new(None),
        kick_tx: kick_tx.clone(),
        _watchers: watchers,
    });
    lock(&inner.entries).insert(entry.identity.id.clone(), entry.clone());
    tokio::spawn(entry_task(Arc::downgrade(inner), Arc::downgrade(&entry), kick_rx));
    let _ = kick_tx.send(()); // initial snapshot
}

/// Per-checkout task: trailing-debounce fs kicks, then compute + publish. Runs
/// syncs sequentially — kicks during a sync accumulate and trigger another pass.
async fn entry_task(
    inner: Weak<DiffSyncInner>,
    entry: Weak<CheckoutEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
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
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else { return };
        sync_entry(&inner, &entry).await;
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

    // chat.branch upkeep — the git-dir watcher covers HEAD, so every snapshot
    // reconciles mismatched rows (repair tick covers dropped events).
    let chats = lock(&entry.chats).clone();
    for chat in &chats {
        if chat.branch.as_deref() != Some(snapshot.branch.as_str()) {
            if let Err(err) = inner.workspace.set_chat_branch(&chat.id, &snapshot.branch) {
                tracing::debug!(chat = %chat.id, error = %err, "diff-sync: branch write failed");
            }
        }
    }

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
            let result = inner
                .http
                .post(&url)
                .bearer_auth(&edge.token)
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
            match diffs.iter_mut().find(|d| d.checkout_id == updated.checkout_id) {
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
/// service tears the loop down.
async fn diff_sync_task(inner: Weak<DiffSyncInner>, mut chats_rx: watch::Receiver<Vec<Chat>>) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            changed = chats_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow_and_update().clone();
                reconcile(&inner, chats).await;
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow().clone();
                reconcile(&inner, chats).await;
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
    let mut child =
        cmd.spawn().map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
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
    Ok(Capture { stdout: out, truncated })
}

fn split_z(value: &[u8]) -> Vec<String> {
    value
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect()
}

fn parse_name_status(value: &[u8]) -> Vec<DiffFileSummary> {
    let fields = split_z(value);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < fields.len() {
        let raw = fields[i].clone();
        i += 1;
        let code = raw.chars().next().unwrap_or('M');
        let Some(first) = fields.get(i).cloned() else { break };
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
    let records: Vec<String> =
        value.split(|b| *b == 0).map(|part| String::from_utf8_lossy(part).to_string()).collect();
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
    if path.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
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
    let body: String =
        lines.iter().map(|line| format!("+{line}")).collect::<Vec<_>>().join("\n");
    let a = quote_patch_path(&format!("a/{path}"));
    let b = quote_patch_path(&format!("b/{path}"));
    format!(
        "diff --git {a} {b}\nnew file mode 100644\n--- /dev/null\n+++ {b}\n@@ -0,0 +1,{} @@\n{body}\n",
        lines.len()
    )
}

/// One bounded atomic snapshot: tracked diff vs HEAD (or the empty tree) with
/// renames, plus untracked files (via `git status --porcelain`, index untouched)
/// as synthesized new-file hunks. 3MiB patch cap with a `truncated` flag; sha256
/// checksum over branch ‖ head ‖ patch ‖ files ‖ truncated.
pub async fn capture_diff(repos: &Repos, root: &Path) -> Result<DiffSnapshot, EngineError> {
    let head = capture_git(root, &["rev-parse", "--verify", "HEAD"], 256)
        .await
        .map(|c| String::from_utf8_lossy(&c.stdout).trim().to_string())
        .unwrap_or_default();
    let base: &str = if head.is_empty() { EMPTY_TREE_SHA } else { &head };
    let branch = repos.current_branch(root).await.unwrap_or_else(|_| "HEAD".into());

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
        &["diff", "--no-ext-diff", "--no-color", "--find-renames", "--unified=3", base, "--"],
        MAX_PATCH_BYTES,
    )
    .await?;
    // Untracked listing via porcelain status; `--no-optional-locks` keeps this
    // read-only (a status-triggered index refresh would re-kick our own watcher).
    let status = capture_git(
        root,
        &["--no-optional-locks", "status", "--porcelain", "-z"],
        2 * 1024 * 1024,
    )
    .await?;

    let mut files = parse_name_status(&names.stdout);
    apply_numstat(&mut files, &nums.stdout);
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();
    let mut truncated =
        tracked.truncated || names.truncated || nums.truncated || status.truncated;

    if tracked.truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Comet diff truncated\n");
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
        let size = tokio::fs::metadata(&full).await.map(|m| m.len()).unwrap_or(0);
        if size > MAX_PATCH_BYTES as u64 {
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
                            (text.split('\n').count()
                                - usize::from(text.ends_with('\n')))
                                as u32
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
