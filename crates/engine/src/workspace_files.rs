//! Authorized filesystem access for the file tree and editor RPC surface.
//!
//! Every operation resolves a synced chat or space to a checkout owned by this
//! device before accepting a workspace-relative path.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zeron_proto::{
    ListWorkspaceDirectoryRequest, ReadWorkspaceFileRequest, SearchWorkspaceFilesRequest,
    WatchWorkspaceFilesRequest, WorkspaceDirectoryPage, WorkspaceEntry, WorkspaceEntryKind,
    WorkspaceFileChange, WorkspaceFileChangeKind, WorkspaceFileChanges,
    WorkspaceFileConflictReason, WorkspaceFileSearchMatch, WorkspaceFileText,
    WorkspaceFileWriteResult, WorkspaceLineEnding, WorkspaceReadOnlyReason, WorkspaceTarget,
    WorkspaceTextEncoding, WorkspaceWritableEncoding, WorkspaceWritableLineEnding,
    WriteWorkspaceFileOutcome, WriteWorkspaceFileRequest,
};
use zeron_rpc::RpcError;

use crate::{Repos, WorkspaceHost};

const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_RELATIVE_PATH_COMPONENTS: usize = 256;
pub const DIRECTORY_PAGE_SIZE: usize = 500;
pub const MAX_DIRECTORY_ENTRIES: usize = 50_000;
pub const MAX_SEARCH_QUERY_CHARS: usize = 256;
pub const MAX_SEARCH_RESULTS: usize = 200;
pub const WORKSPACE_FILE_RPC_TIMEOUT: Duration = Duration::from_secs(6);
pub const MAX_EDITABLE_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_PREVIEW_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);
pub const WATCH_MAX_BURST: Duration = Duration::from_secs(1);
pub const WATCH_REPAIR_INTERVAL: Duration = Duration::from_secs(120);
pub const MAX_WATCH_DIRS: usize = 8_000;
const WATCH_EVENT_BUFFER: usize = 256;
const WATCH_BROADCAST_BUFFER: usize = 64;

#[derive(Clone)]
pub struct WorkspaceFiles {
    inner: Arc<WorkspaceFilesInner>,
}

struct WorkspaceFilesInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    write_locks: Mutex<HashMap<WorkspaceFileKey, Weak<tokio::sync::Mutex<()>>>>,
    watches: Mutex<HashMap<String, Arc<CheckoutWatch>>>,
    cancel: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkspaceFileKey {
    checkout_id: String,
    path: PathBuf,
}

struct CheckoutWatch {
    checkout_id: String,
    root: PathBuf,
    sequence: AtomicU64,
    subscribers: AtomicUsize,
    changes_tx: broadcast::Sender<WorkspaceFileChanges>,
    cancel: CancellationToken,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub struct WorkspaceFileSubscription {
    receiver: broadcast::Receiver<WorkspaceFileChanges>,
    watch: Arc<CheckoutWatch>,
    owner: Weak<WorkspaceFilesInner>,
    initial: Option<WorkspaceFileChanges>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedWorkspace {
    pub checkout_id: String,
    pub root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceFilesError {
    #[error("{0}")]
    BadParams(String),
    #[error("{0}")]
    Authorization(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Io(String),
}

impl From<WorkspaceFilesError> for RpcError {
    fn from(error: WorkspaceFilesError) -> Self {
        match error {
            WorkspaceFilesError::BadParams(message) => RpcError::BadParams(message),
            WorkspaceFilesError::Authorization(message)
            | WorkspaceFilesError::NotFound(message)
            | WorkspaceFilesError::Unsupported(message)
            | WorkspaceFilesError::Io(message) => RpcError::Failed(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceRelativePath(PathBuf);

impl WorkspaceRelativePath {
    pub fn directory(path: &str) -> Result<Self, WorkspaceFilesError> {
        Self::parse(path, true)
    }

    pub fn file(path: &str) -> Result<Self, WorkspaceFilesError> {
        Self::parse(path, false)
    }

    fn parse(path: &str, allow_root: bool) -> Result<Self, WorkspaceFilesError> {
        if path.is_empty() {
            return allow_root
                .then(|| Self(PathBuf::new()))
                .ok_or_else(|| bad_path("path must not be empty"));
        }
        if path.len() > MAX_RELATIVE_PATH_BYTES {
            return Err(bad_path("path is too long"));
        }
        if path.contains(['\0', '\\', ':']) {
            return Err(bad_path("path contains an invalid character"));
        }
        if path.starts_with('/') || path.starts_with("//") {
            return Err(bad_path("path must be workspace-relative"));
        }
        if path
            .split('/')
            .any(|component| component == "." || component == "..")
        {
            return Err(bad_path("path must not contain . or .."));
        }

        let parsed = Path::new(path);
        let mut count = 0usize;
        for component in parsed.components() {
            count += 1;
            if count > MAX_RELATIVE_PATH_COMPONENTS {
                return Err(bad_path("path has too many components"));
            }
            match component {
                Component::Normal(value) => {
                    let value = value
                        .to_str()
                        .ok_or_else(|| bad_path("path must be UTF-8"))?;
                    if value.eq_ignore_ascii_case(".git") {
                        return Err(bad_path(".git paths are not accessible"));
                    }
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(bad_path("path must not contain . or .."));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(bad_path("path must be workspace-relative"));
                }
            }
        }
        Ok(Self(parsed.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn wire_path(&self) -> String {
        self.0
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => value.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

fn bad_path(message: &str) -> WorkspaceFilesError {
    WorkspaceFilesError::BadParams(message.to_string())
}

fn plain_folder_identity(device_id: &str, root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_id.as_bytes());
    hasher.update([0]);
    hasher.update(b"plain-folder");
    hasher.update([0]);
    hasher.update(root.to_string_lossy().as_bytes());
    format!("folder-{}", hex(&hasher.finalize()))
}

impl WorkspaceFiles {
    pub fn new(repos: Repos, workspace: WorkspaceHost, device_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(WorkspaceFilesInner {
                repos,
                workspace,
                device_id: device_id.into(),
                write_locks: Mutex::new(HashMap::new()),
                watches: Mutex::new(HashMap::new()),
                cancel: CancellationToken::new(),
            }),
        }
    }

    pub(crate) async fn resolve_target(
        &self,
        target: &WorkspaceTarget,
    ) -> Result<ResolvedWorkspace, WorkspaceFilesError> {
        let root = match (&target.chat_id, &target.space_id) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(WorkspaceFilesError::BadParams(
                    "workspace target needs exactly one of chatId or spaceId".into(),
                ));
            }
            (Some(chat_id), None) => {
                if target.checkout_path.is_some() {
                    return Err(WorkspaceFilesError::BadParams(
                        "checkoutPath applies only to a space target".into(),
                    ));
                }
                let chat = self
                    .inner
                    .workspace
                    .chat(chat_id)
                    .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?
                    .ok_or_else(|| WorkspaceFilesError::NotFound("chat not found".into()))?;
                if chat.device_id != self.inner.device_id {
                    return Err(WorkspaceFilesError::Authorization(
                        "chat belongs to another device".into(),
                    ));
                }
                let cwd = chat.cwd.map(PathBuf::from).ok_or_else(|| {
                    WorkspaceFilesError::NotFound("chat has no workspace folder".into())
                })?;
                let space_id = chat.space_id.ok_or_else(|| {
                    WorkspaceFilesError::NotFound("chat has no workspace space".into())
                })?;
                let space = self
                    .inner
                    .workspace
                    .space(&space_id)
                    .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?
                    .ok_or_else(|| {
                        WorkspaceFilesError::NotFound("chat workspace space not found".into())
                    })?;
                if space.device_id != self.inner.device_id {
                    return Err(WorkspaceFilesError::Authorization(
                        "chat space belongs to another device".into(),
                    ));
                }
                self.inner
                    .repos
                    .workspace_checkout(Path::new(&space.path), &cwd)
                    .await
                    .ok_or_else(|| {
                        WorkspaceFilesError::Authorization(
                            "chat folder is not a workspace checkout".into(),
                        )
                    })?
            }
            (None, Some(space_id)) => {
                let space = self
                    .inner
                    .workspace
                    .space(space_id)
                    .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?
                    .ok_or_else(|| WorkspaceFilesError::NotFound("space not found".into()))?;
                if space.device_id != self.inner.device_id {
                    return Err(WorkspaceFilesError::Authorization(
                        "space belongs to another device".into(),
                    ));
                }
                let space_path = PathBuf::from(&space.path);
                let requested = target
                    .checkout_path
                    .as_deref()
                    .map_or_else(|| space_path.clone(), PathBuf::from);
                self.inner
                    .repos
                    .workspace_checkout(&space_path, &requested)
                    .await
                    .ok_or_else(|| {
                        WorkspaceFilesError::BadParams(
                            "checkoutPath is not a workspace checkout".into(),
                        )
                    })?
            }
        };

        // Spaces also support plain folders. Git checkouts use their canonical
        // git-dir identity; plain roots get a device-scoped stable key so the
        // existing SearchFiles behavior and shared watcher semantics remain intact.
        match self.inner.repos.checkout_identity(&root).await {
            Ok(identity) => Ok(ResolvedWorkspace {
                checkout_id: identity.id,
                root: identity.root,
            }),
            Err(_) => Ok(ResolvedWorkspace {
                checkout_id: plain_folder_identity(&self.inner.device_id, &root),
                root,
            }),
        }
    }

    pub async fn list_directory(
        &self,
        request: ListWorkspaceDirectoryRequest,
    ) -> Result<WorkspaceDirectoryPage, WorkspaceFilesError> {
        let workspace = self.resolve_target(&request.target).await?;
        let directory = WorkspaceRelativePath::directory(&request.directory)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let root = workspace.root;
        let result = tokio::task::spawn_blocking(move || {
            list_directory_blocking(
                &root,
                &directory,
                request.include_ignored,
                request.cursor.as_deref(),
                &cancel,
            )
        })
        .await
        .map_err(|error| WorkspaceFilesError::Io(format!("directory worker failed: {error}")))?;
        cancel_on_drop.disarm();
        result
    }

    pub async fn search(
        &self,
        request: SearchWorkspaceFilesRequest,
    ) -> Result<Vec<WorkspaceFileSearchMatch>, WorkspaceFilesError> {
        validate_workspace_search_query(&request.query)?;
        let workspace = self.resolve_target(&request.target).await?;
        let limit =
            usize::from(request.limit.unwrap_or(MAX_SEARCH_RESULTS as u16)).min(MAX_SEARCH_RESULTS);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let result = tokio::task::spawn_blocking(move || {
            search_workspace_blocking(
                &workspace.root,
                &request.query,
                request.include_ignored,
                limit,
                &cancel,
            )
        })
        .await
        .map_err(|error| WorkspaceFilesError::Io(format!("search worker failed: {error}")))?;
        cancel_on_drop.disarm();
        result
    }

    pub async fn read_file(
        &self,
        request: ReadWorkspaceFileRequest,
    ) -> Result<WorkspaceFileText, WorkspaceFilesError> {
        let workspace = self.resolve_target(&request.target).await?;
        let relative = WorkspaceRelativePath::file(&request.path)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let result = tokio::task::spawn_blocking(move || {
            let mut file = read_file_blocking(&workspace.root, &relative, &cancel)?;
            file.checkout_id = workspace.checkout_id;
            Ok(file)
        })
        .await
        .map_err(|error| WorkspaceFilesError::Io(format!("file read worker failed: {error}")))?;
        cancel_on_drop.disarm();
        result
    }

    pub async fn read_image(
        &self,
        request: zeron_proto::ReadWorkspaceImageRequest,
    ) -> Result<zeron_proto::WorkspaceImageChunk, WorkspaceFilesError> {
        let workspace = self.resolve_target(&request.target).await?;
        if request.expected_checkout_id.is_empty()
            || request.expected_checkout_id != workspace.checkout_id
        {
            return Err(WorkspaceFilesError::Authorization(
                "Workspace changed before image read".into(),
            ));
        }
        let relative = WorkspaceRelativePath::file(&request.path)?;
        tokio::task::spawn_blocking(move || {
            read_image_blocking(&workspace.root, &relative, &request)
        })
        .await
        .map_err(|e| WorkspaceFilesError::Io(e.to_string()))?
    }

    pub async fn write_file(
        &self,
        request: WriteWorkspaceFileRequest,
    ) -> Result<WriteWorkspaceFileOutcome, WorkspaceFilesError> {
        let bytes = encode_write_text(&request)?;
        let workspace = self.resolve_target(&request.target).await?;
        if request.expected_checkout_id.is_empty()
            || request.expected_checkout_id != workspace.checkout_id
        {
            return Err(WorkspaceFilesError::Authorization(
                "Workspace changed since this file was opened. Switch back to save your edits."
                    .into(),
            ));
        }
        let relative = WorkspaceRelativePath::file(&request.path)?;
        let key = WorkspaceFileKey {
            checkout_id: workspace.checkout_id.clone(),
            path: relative.as_path().to_path_buf(),
        };
        let file_lock = {
            let mut locks = lock(&self.inner.write_locks);
            locks.retain(|_, slot| slot.strong_count() > 0);
            if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
                existing
            } else {
                let file_lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&file_lock));
                file_lock
            }
        };
        let write_guard = file_lock.lock_owned().await;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let expected_hash = request.expected_content_hash;
        let result = tokio::task::spawn_blocking(move || {
            let _write_guard = write_guard;
            write_file_blocking(&workspace.root, &relative, &expected_hash, &bytes, &cancel)
        })
        .await
        .map_err(|error| WorkspaceFilesError::Io(format!("file write worker failed: {error}")))?;
        cancel_on_drop.disarm();
        result
    }

    pub async fn watch_files(
        &self,
        request: WatchWorkspaceFilesRequest,
    ) -> Result<WorkspaceFileSubscription, WorkspaceFilesError> {
        if self.inner.cancel.is_cancelled() {
            return Err(WorkspaceFilesError::Io(
                "workspace file service is shutting down".into(),
            ));
        }
        let workspace = self.resolve_target(&request.target).await?;
        {
            let mut watches = lock(&self.inner.watches);
            if let Some(existing) = watches.get(&workspace.checkout_id).cloned() {
                if !existing.cancel.is_cancelled() {
                    return Ok(existing.subscribe(Arc::downgrade(&self.inner)));
                }
                watches.remove(&workspace.checkout_id);
            }
        }

        let root = workspace.root.clone();
        let over_budget = tokio::task::spawn_blocking(move || exceeds_watch_budget(&root))
            .await
            .map_err(|error| {
                WorkspaceFilesError::Io(format!("watch budget worker failed: {error}"))
            })?;
        let candidate = CheckoutWatch::start(
            workspace.checkout_id.clone(),
            workspace.root,
            over_budget,
            self.inner.cancel.child_token(),
        );
        let subscription = {
            let mut watches = lock(&self.inner.watches);
            if let Some(existing) = watches.get(&workspace.checkout_id) {
                candidate.cancel.cancel();
                lock(&candidate.watcher).take();
                existing.subscribe(Arc::downgrade(&self.inner))
            } else {
                watches.insert(workspace.checkout_id, candidate.clone());
                candidate.subscribe(Arc::downgrade(&self.inner))
            }
        };
        Ok(subscription)
    }

    /// Cancel all service-owned work. This operation is idempotent.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let watches: Vec<_> = {
            let mut watches = lock(&self.inner.watches);
            let values = watches.values().cloned().collect();
            watches.clear();
            values
        };
        let mut tasks = Vec::new();
        for watch in watches {
            watch.cancel.cancel();
            lock(&watch.watcher).take();
            if let Some(task) = lock(&watch.task).take() {
                tasks.push(task);
            }
        }
        for task in tasks {
            let _ = task.await;
        }
        lock(&self.inner.write_locks).clear();
    }
}

impl CheckoutWatch {
    fn start(
        checkout_id: String,
        root: PathBuf,
        over_budget: bool,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let (changes_tx, _) = broadcast::channel(WATCH_BROADCAST_BUFFER);
        let (event_tx, event_rx) = mpsc::channel(WATCH_EVENT_BUFFER);
        let overflow = Arc::new(AtomicBool::new(false));
        let overflow_notify = Arc::new(Notify::new());
        let watcher = if over_budget {
            None
        } else {
            let callback_overflow = overflow.clone();
            let callback_notify = overflow_notify.clone();
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if event
                    .as_ref()
                    .is_ok_and(|event| matches!(event.kind, notify::EventKind::Access(_)))
                {
                    return;
                }
                let event = TimedWatchEvent {
                    received_at: Instant::now(),
                    event,
                };
                match event_tx.try_send(event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        callback_overflow.store(true, Ordering::Release);
                        callback_notify.notify_one();
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            })
            .ok()
            .and_then(|mut watcher| {
                use notify::Watcher as _;
                watcher
                    .watch(&root, notify::RecursiveMode::Recursive)
                    .ok()
                    .map(|()| watcher)
            })
        };
        let repair_only = over_budget || watcher.is_none();
        let watch = Arc::new(Self {
            checkout_id,
            root,
            sequence: AtomicU64::new(0),
            subscribers: AtomicUsize::new(0),
            changes_tx,
            cancel,
            watcher: Mutex::new(watcher),
            task: Mutex::new(None),
        });
        let task = tokio::spawn(watch_task(
            Arc::downgrade(&watch),
            event_rx,
            overflow,
            overflow_notify,
            repair_only,
        ));
        *lock(&watch.task) = Some(task);
        watch
    }

    fn subscribe(self: &Arc<Self>, owner: Weak<WorkspaceFilesInner>) -> WorkspaceFileSubscription {
        self.subscribers.fetch_add(1, Ordering::AcqRel);
        let initial = self.resync();
        WorkspaceFileSubscription {
            receiver: self.changes_tx.subscribe(),
            watch: self.clone(),
            owner,
            initial: Some(initial),
        }
    }

    fn resync(&self) -> WorkspaceFileChanges {
        WorkspaceFileChanges {
            sequence: self.sequence.fetch_add(1, Ordering::AcqRel) + 1,
            resync_required: true,
            changes: Vec::new(),
        }
    }

    fn publish(&self, resync_required: bool, changes: Vec<WorkspaceFileChange>) {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::trace!(
            checkout_id = %self.checkout_id,
            sequence,
            resync_required,
            change_count = changes.len(),
            "workspace file watcher publishing changes"
        );
        let _ = self.changes_tx.send(WorkspaceFileChanges {
            sequence,
            resync_required,
            changes,
        });
    }
}

impl WorkspaceFileSubscription {
    pub async fn recv(&mut self) -> Option<WorkspaceFileChanges> {
        if self.watch.cancel.is_cancelled() {
            return None;
        }
        if let Some(initial) = self.initial.take() {
            return Some(initial);
        }
        let received = tokio::select! {
            _ = self.watch.cancel.cancelled() => return None,
            received = self.receiver.recv() => received,
        };
        match received {
            Ok(changes) => Some(changes),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(self.watch.resync()),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

impl Drop for WorkspaceFileSubscription {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            let mut watches = lock(&owner.watches);
            let last = self.watch.subscribers.fetch_sub(1, Ordering::AcqRel) == 1;
            if last
                && watches
                    .get(&self.watch.checkout_id)
                    .is_some_and(|watch| Arc::ptr_eq(watch, &self.watch))
            {
                watches.remove(&self.watch.checkout_id);
            }
            drop(watches);
            if last {
                self.watch.cancel.cancel();
                lock(&self.watch.watcher).take();
            }
            return;
        }
        if self.watch.subscribers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.watch.cancel.cancel();
            lock(&self.watch.watcher).take();
        }
    }
}

struct TimedWatchEvent {
    received_at: Instant,
    event: Result<notify::Event, notify::Error>,
}

async fn watch_task(
    watch: Weak<CheckoutWatch>,
    mut event_rx: mpsc::Receiver<TimedWatchEvent>,
    overflow: Arc<AtomicBool>,
    overflow_notify: Arc<Notify>,
    repair_only: bool,
) {
    let Some(initial) = watch.upgrade() else {
        return;
    };
    let cancel = initial.cancel.clone();
    drop(initial);
    let mut repair = tokio::time::interval(WATCH_REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await;
    loop {
        enum Wake {
            Event(TimedWatchEvent),
            Overflow,
            Repair,
        }
        let wake = tokio::select! {
            _ = cancel.cancelled() => return,
            _ = overflow_notify.notified() => Wake::Overflow,
            _ = repair.tick() => Wake::Repair,
            event = event_rx.recv(), if !repair_only => match event {
                Some(event) => Wake::Event(event),
                None => return,
            },
        };
        let Some(watch) = watch.upgrade() else {
            return;
        };
        match wake {
            Wake::Overflow => {
                overflow.store(false, Ordering::Release);
                tracing::trace!(
                    checkout_id = %watch.checkout_id,
                    "workspace file watcher overflow requested resync"
                );
                watch.publish(true, Vec::new());
            }
            Wake::Repair => {
                overflow.store(false, Ordering::Release);
                tracing::trace!(
                    checkout_id = %watch.checkout_id,
                    "workspace file watcher repair requested resync"
                );
                watch.publish(true, Vec::new());
            }
            Wake::Event(first) => {
                let burst_started = first.received_at;
                let mut events = vec![first.event];
                loop {
                    let remaining = WATCH_MAX_BURST.saturating_sub(burst_started.elapsed());
                    if remaining.is_zero() {
                        break;
                    }
                    match tokio::time::timeout(WATCH_DEBOUNCE.min(remaining), event_rx.recv()).await
                    {
                        Ok(Some(event)) => events.push(event.event),
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                let overflowed = overflow.swap(false, Ordering::AcqRel);
                let event_count = events.len();
                let (mut resync_required, changes) = normalize_watch_events(&watch.root, events);
                resync_required |= overflowed;
                tracing::trace!(
                    checkout_id = %watch.checkout_id,
                    event_count,
                    change_count = changes.len(),
                    resync_required,
                    burst_ms = burst_started.elapsed().as_millis(),
                    "workspace file watcher normalized event burst"
                );
                if resync_required || !changes.is_empty() {
                    watch.publish(resync_required, changes);
                }
            }
        }
    }
}

fn normalize_watch_events(
    root: &Path,
    events: Vec<Result<notify::Event, notify::Error>>,
) -> (bool, Vec<WorkspaceFileChange>) {
    use notify::EventKind;
    use notify::event::{ModifyKind, RenameMode};

    let mut resync_required = false;
    let mut changes: HashMap<String, WorkspaceFileChange> = HashMap::new();
    for event in events {
        let event = match event {
            Ok(event) => event,
            Err(_) => {
                resync_required = true;
                continue;
            }
        };
        if matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::Both))
        ) && event.paths.len() >= 2
        {
            let old_path = normalize_watch_path_including_temp(root, &event.paths[0]);
            let path = event
                .paths
                .last()
                .and_then(|path| normalize_watch_path(root, path));
            if let (Some(old_path), Some(path)) = (old_path, path) {
                if is_internal_temp_wire_path(&old_path) {
                    changes.insert(
                        path.clone(),
                        WorkspaceFileChange {
                            kind: WorkspaceFileChangeKind::Modified,
                            path,
                            old_path: None,
                        },
                    );
                    continue;
                }
                changes.insert(
                    path.clone(),
                    WorkspaceFileChange {
                        kind: WorkspaceFileChangeKind::Renamed,
                        path,
                        old_path: Some(old_path),
                    },
                );
            }
            continue;
        }
        let kind = match event.kind {
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                Some(WorkspaceFileChangeKind::Created)
            }
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                Some(WorkspaceFileChangeKind::Removed)
            }
            EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
                Some(WorkspaceFileChangeKind::Modified)
            }
            EventKind::Access(_) => None,
        };
        let Some(kind) = kind else { continue };
        for path in event.paths {
            let Some(path) = normalize_watch_path(root, &path) else {
                continue;
            };
            let incoming = WorkspaceFileChange {
                kind,
                path: path.clone(),
                old_path: None,
            };
            match changes.get(&path) {
                // A file removed and recreated inside one debounce window exists
                // again. Preserve the final state so open documents can recover.
                Some(existing)
                    if existing.kind == WorkspaceFileChangeKind::Removed
                        && kind == WorkspaceFileChangeKind::Created =>
                {
                    changes.insert(path, incoming);
                }
                Some(existing)
                    if watch_change_priority(existing.kind) > watch_change_priority(kind) => {}
                _ => {
                    changes.insert(path, incoming);
                }
            }
        }
    }
    let mut changes: Vec<_> = changes.into_values().collect();
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    (resync_required, changes)
}

fn watch_change_priority(kind: WorkspaceFileChangeKind) -> u8 {
    match kind {
        WorkspaceFileChangeKind::Modified => 0,
        WorkspaceFileChangeKind::Created => 1,
        WorkspaceFileChangeKind::Renamed => 2,
        WorkspaceFileChangeKind::Removed => 3,
    }
}

fn normalize_watch_path(root: &Path, path: &Path) -> Option<String> {
    let path = normalize_watch_path_including_temp(root, path)?;
    (!is_internal_temp_wire_path(&path)).then_some(path)
}

fn normalize_watch_path_including_temp(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() || contains_git_component(relative) {
        return None;
    }
    path_to_wire(relative).ok()
}

fn is_internal_temp_wire_path(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with(".zeron-save-") && name.ends_with(".tmp"))
}

fn exceeds_watch_budget(root: &Path) -> bool {
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut seen = 0usize;
    while let Some(directory) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
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

struct CancelOnDrop(Option<Arc<AtomicBool>>);

impl CancelOnDrop {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        Self(Some(cancel))
    }

    fn disarm(mut self) {
        self.0.take();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectoryCursor {
    version: u8,
    directory: String,
    include_ignored: bool,
    offset: usize,
    fingerprint: String,
}

fn list_directory_blocking(
    root: &Path,
    directory: &WorkspaceRelativePath,
    include_ignored: bool,
    cursor: Option<&str>,
    cancel: &AtomicBool,
) -> Result<WorkspaceDirectoryPage, WorkspaceFilesError> {
    let target = checked_directory(root, directory)?;
    let visible_paths = include_ignored.then(|| filtered_directory_paths(root, &target));
    let mut builder = ignore::WalkBuilder::new(&target);
    builder.max_depth(Some(1)).follow_links(false).hidden(false);
    if include_ignored {
        builder.standard_filters(false);
    }

    let mut entries = Vec::new();
    let mut hard_truncated = false;
    for result in builder.build() {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkspaceFilesError::Io(
                "directory listing cancelled".into(),
            ));
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(error) if entries.is_empty() => {
                return Err(WorkspaceFilesError::Io(error.to_string()));
            }
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| WorkspaceFilesError::Authorization("entry escaped workspace".into()))?;
        if contains_git_component(relative) {
            continue;
        }
        if path_to_wire(relative)
            .ok()
            .is_some_and(|path| is_internal_temp_wire_path(&path))
        {
            continue;
        }
        if entries.len() == MAX_DIRECTORY_ENTRIES {
            hard_truncated = true;
            break;
        }
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(WorkspaceFilesError::Io(error.to_string())),
        };
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            WorkspaceEntryKind::Symlink
        } else if file_type.is_dir() {
            WorkspaceEntryKind::Directory
        } else {
            WorkspaceEntryKind::File
        };
        let path = path_to_wire(relative)?;
        let ignored = visible_paths
            .as_ref()
            .is_some_and(|visible| !visible.contains(&path));
        entries.push(WorkspaceEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            kind,
            size: file_type.is_file().then_some(metadata.len()),
            modified_at: metadata.modified().ok().map(chrono::DateTime::from),
            ignored,
            read_only: file_type.is_symlink() || !file_type.is_file(),
        });
    }

    entries.sort_by(|left, right| {
        entry_group(left.kind)
            .cmp(&entry_group(right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    let fingerprint = directory_fingerprint(&entries);
    let offset = if let Some(cursor) = cursor {
        let cursor = decode_cursor(cursor)?;
        if cursor.version != 1
            || cursor.directory != directory.wire_path()
            || cursor.include_ignored != include_ignored
        {
            return Err(WorkspaceFilesError::BadParams(
                "directory cursor does not match this request; restart listing".into(),
            ));
        }
        if cursor.fingerprint != fingerprint {
            return Err(WorkspaceFilesError::BadParams(
                "directory changed between pages; restart listing".into(),
            ));
        }
        cursor.offset
    } else {
        0
    };
    if offset > entries.len() {
        return Err(WorkspaceFilesError::BadParams(
            "directory cursor is out of range; restart listing".into(),
        ));
    }
    let end = (offset + DIRECTORY_PAGE_SIZE).min(entries.len());
    let page_entries = entries[offset..end].to_vec();
    let next_cursor = (end < entries.len()).then(|| {
        encode_cursor(&DirectoryCursor {
            version: 1,
            directory: directory.wire_path(),
            include_ignored,
            offset: end,
            fingerprint,
        })
    });
    Ok(WorkspaceDirectoryPage {
        directory: directory.wire_path(),
        entries: page_entries,
        next_cursor,
        truncated: hard_truncated,
    })
}

fn filtered_directory_paths(root: &Path, target: &Path) -> HashSet<String> {
    let mut builder = ignore::WalkBuilder::new(target);
    builder.max_depth(Some(1)).follow_links(false).hidden(false);
    builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(|path| path_to_wire(path).ok())
        })
        .collect()
}

fn search_workspace_blocking(
    root: &Path,
    query: &str,
    include_ignored: bool,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<Vec<WorkspaceFileSearchMatch>, WorkspaceFilesError> {
    validate_workspace_search_query(query)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut builder = ignore::WalkBuilder::new(root);
    builder.follow_links(false).hidden(false);
    if include_ignored {
        builder.standard_filters(false);
    }
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    for result in builder.build() {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkspaceFilesError::Io("workspace search cancelled".into()));
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(error) if matches.is_empty() => {
                return Err(WorkspaceFilesError::Io(error.to_string()));
            }
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let relative = match entry.path().strip_prefix(root) {
            Ok(relative) if !contains_git_component(relative) => relative,
            _ => continue,
        };
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        let kind = if file_type.is_symlink() {
            WorkspaceEntryKind::Symlink
        } else if file_type.is_dir() {
            WorkspaceEntryKind::Directory
        } else if file_type.is_file() {
            WorkspaceEntryKind::File
        } else {
            continue;
        };
        let path = path_to_wire(relative)?;
        if is_internal_temp_wire_path(&path) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(score) = workspace_search_score(&name, &path, &query_lower) else {
            continue;
        };
        matches.push(WorkspaceFileSearchMatch {
            path,
            name,
            kind,
            score,
        });
        if matches.len() > limit {
            matches.sort_by(compare_workspace_search_matches);
            matches.truncate(limit);
        }
    }
    matches.sort_by(compare_workspace_search_matches);
    Ok(matches)
}

fn validate_workspace_search_query(query: &str) -> Result<(), WorkspaceFilesError> {
    if query.trim().is_empty() {
        return Err(WorkspaceFilesError::BadParams(
            "query must not be empty".into(),
        ));
    }
    if query.chars().count() > MAX_SEARCH_QUERY_CHARS {
        return Err(WorkspaceFilesError::BadParams(format!(
            "query must not exceed {MAX_SEARCH_QUERY_CHARS} characters"
        )));
    }
    Ok(())
}

fn compare_workspace_search_matches(
    left: &WorkspaceFileSearchMatch,
    right: &WorkspaceFileSearchMatch,
) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.to_lowercase().cmp(&right.path.to_lowercase()))
        .then_with(|| left.path.cmp(&right.path))
}

fn read_image_blocking(
    root: &Path,
    relative: &WorkspaceRelativePath,
    request: &zeron_proto::ReadWorkspaceImageRequest,
) -> Result<zeron_proto::WorkspaceImageChunk, WorkspaceFilesError> {
    use base64::Engine as _;
    use std::io::Read;
    use zeron_proto::{MAX_WORKSPACE_IMAGE_BYTES, WORKSPACE_IMAGE_CHUNK_BYTES};
    let mime = match relative
        .as_path()
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => {
            return Err(WorkspaceFilesError::Unsupported(
                "Unsupported workspace image format".into(),
            ));
        }
    };
    if request.offset > 0 && request.expected_content_hash.is_none() {
        return Err(bad_path("Image continuation requires a content hash"));
    }
    let before = checked_file_metadata(root, relative)?;
    if before.len() > MAX_WORKSPACE_IMAGE_BYTES as u64 {
        return Err(WorkspaceFilesError::Unsupported(
            "Image exceeds 8 MiB preview limit".into(),
        ));
    }
    let file = std::fs::File::open(root.join(relative.as_path()))
        .map_err(|e| WorkspaceFilesError::Io(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_WORKSPACE_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| WorkspaceFilesError::Io(e.to_string()))?;
    let after = checked_file_metadata(root, relative)?;
    if bytes.len() > MAX_WORKSPACE_IMAGE_BYTES
        || !same_file_revision(&before, &after)
        || bytes.len() as u64 != after.len()
    {
        return Err(WorkspaceFilesError::Io(
            "Image changed during read or exceeds preview limit".into(),
        ));
    }
    let hash = hash_bytes(&bytes);
    if request
        .expected_content_hash
        .as_ref()
        .is_some_and(|expected| expected != &hash)
    {
        return Err(WorkspaceFilesError::Io(
            "Image changed between chunks; reload preview".into(),
        ));
    }
    if request.offset > bytes.len() {
        return Err(bad_path("Invalid image offset"));
    }
    let end = request
        .offset
        .saturating_add(WORKSPACE_IMAGE_CHUNK_BYTES)
        .min(bytes.len());
    Ok(zeron_proto::WorkspaceImageChunk {
        checkout_id: request.expected_checkout_id.clone(),
        content_hash: hash,
        mime_type: mime.into(),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes[request.offset..end]),
        next_offset: end,
        size: bytes.len(),
        done: end == bytes.len(),
    })
}

fn read_file_blocking(
    root: &Path,
    relative: &WorkspaceRelativePath,
    cancel: &AtomicBool,
) -> Result<WorkspaceFileText, WorkspaceFilesError> {
    let path = root.join(relative.as_path());
    let wire_path = relative.wire_path();
    let metadata = match checked_file_metadata(root, relative) {
        Ok(metadata) => metadata,
        Err(WorkspaceFilesError::Unsupported(message)) if message == "path is a symlink" => {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
            return Ok(non_text_file(
                wire_path,
                &metadata,
                WorkspaceReadOnlyReason::Symlink,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(message))
            if message == "path is not a regular file" =>
        {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
            return Ok(non_text_file(
                wire_path,
                &metadata,
                WorkspaceReadOnlyReason::NotRegularFile,
            ));
        }
        Err(error) => return Err(error),
    };
    if metadata.len() > MAX_PREVIEW_FILE_BYTES {
        return Ok(WorkspaceFileText {
            checkout_id: String::new(),
            path: wire_path,
            text: None,
            content_hash: None,
            size: metadata.len(),
            modified_at: metadata.modified().ok().map(chrono::DateTime::from),
            encoding: WorkspaceTextEncoding::Unsupported,
            line_ending: None,
            read_only_reason: Some(WorkspaceReadOnlyReason::TooLarge),
            truncated: true,
        });
    }

    for attempt in 0..2 {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkspaceFilesError::Io("file read cancelled".into()));
        }
        let before = checked_file_metadata(root, relative)?;
        let bytes =
            std::fs::read(&path).map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
        let after = checked_file_metadata(root, relative)?;
        if same_file_revision(&before, &after) && bytes.len() as u64 == after.len() {
            return Ok(classify_file_text(wire_path, bytes, &after));
        }
        if attempt == 1 {
            return Err(WorkspaceFilesError::Io(
                "file changed while it was being read; retry".into(),
            ));
        }
    }
    unreachable!("read retry loop always returns")
}

fn checked_file_metadata(
    root: &Path,
    relative: &WorkspaceRelativePath,
) -> Result<std::fs::Metadata, WorkspaceFilesError> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.as_path().components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err(bad_path("invalid file component"));
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceFilesError::NotFound("file not found".into())
            } else {
                WorkspaceFilesError::Io(error.to_string())
            }
        })?;
        if metadata.file_type().is_symlink() {
            let message = if index + 1 == components.len() {
                "path is a symlink"
            } else {
                "path traverses a symlink"
            };
            return Err(WorkspaceFilesError::Unsupported(message.into()));
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Err(WorkspaceFilesError::Unsupported(
                "file parent is not a directory".into(),
            ));
        }
        if index + 1 == components.len() && !metadata.is_file() {
            return Err(WorkspaceFilesError::Unsupported(
                "path is not a regular file".into(),
            ));
        }
    }
    let canonical = std::fs::canonicalize(&current)
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    if !canonical.starts_with(root) {
        return Err(WorkspaceFilesError::Authorization(
            "file escaped workspace".into(),
        ));
    }
    std::fs::symlink_metadata(canonical).map_err(|error| WorkspaceFilesError::Io(error.to_string()))
}

fn non_text_file(
    path: String,
    metadata: &std::fs::Metadata,
    reason: WorkspaceReadOnlyReason,
) -> WorkspaceFileText {
    WorkspaceFileText {
        checkout_id: String::new(),
        path,
        text: None,
        content_hash: None,
        size: metadata.len(),
        modified_at: metadata.modified().ok().map(chrono::DateTime::from),
        encoding: WorkspaceTextEncoding::Unsupported,
        line_ending: None,
        read_only_reason: Some(reason),
        truncated: false,
    }
}

fn classify_file_text(
    path: String,
    bytes: Vec<u8>,
    metadata: &std::fs::Metadata,
) -> WorkspaceFileText {
    let content_hash = Some(hash_bytes(&bytes));
    let size = bytes.len() as u64;
    let modified_at = metadata.modified().ok().map(chrono::DateTime::from);
    if bytes.contains(&0) {
        return WorkspaceFileText {
            checkout_id: String::new(),
            path,
            text: None,
            content_hash,
            size,
            modified_at,
            encoding: WorkspaceTextEncoding::Binary,
            line_ending: None,
            read_only_reason: Some(WorkspaceReadOnlyReason::Binary),
            truncated: false,
        };
    }

    let (encoding, source) = if let Some(source) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        (WorkspaceTextEncoding::Utf8Bom, source)
    } else {
        (WorkspaceTextEncoding::Utf8, bytes.as_slice())
    };
    let Ok(source) = std::str::from_utf8(source) else {
        return WorkspaceFileText {
            checkout_id: String::new(),
            path,
            text: None,
            content_hash,
            size,
            modified_at,
            encoding: WorkspaceTextEncoding::Unsupported,
            line_ending: None,
            read_only_reason: Some(WorkspaceReadOnlyReason::UnsupportedEncoding),
            truncated: false,
        };
    };
    let line_ending = detect_line_ending(source.as_bytes());
    let text = match line_ending {
        WorkspaceLineEnding::Crlf => source.replace("\r\n", "\n"),
        _ => source.to_string(),
    };
    let read_only_reason = if size > MAX_EDITABLE_FILE_BYTES {
        Some(WorkspaceReadOnlyReason::TooLarge)
    } else if line_ending == WorkspaceLineEnding::Mixed {
        Some(WorkspaceReadOnlyReason::MixedLineEndings)
    } else {
        None
    };
    WorkspaceFileText {
        checkout_id: String::new(),
        path,
        text: Some(text),
        content_hash,
        size,
        modified_at,
        encoding,
        line_ending: Some(line_ending),
        read_only_reason,
        truncated: false,
    }
}

fn detect_line_ending(bytes: &[u8]) -> WorkspaceLineEnding {
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut lone_cr = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf += 1;
                index += 2;
            }
            b'\r' => {
                lone_cr += 1;
                index += 1;
            }
            b'\n' => {
                lf += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    match (crlf, lf, lone_cr) {
        (0, 0, 0) => WorkspaceLineEnding::None,
        (0, _, 0) => WorkspaceLineEnding::Lf,
        (_, 0, 0) => WorkspaceLineEnding::Crlf,
        _ => WorkspaceLineEnding::Mixed,
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn same_file_revision(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev() && before.ino() == after.ino()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn encode_write_text(request: &WriteWorkspaceFileRequest) -> Result<Vec<u8>, WorkspaceFilesError> {
    if request.text.contains('\0') {
        return Err(WorkspaceFilesError::BadParams(
            "write text must not contain NUL bytes".into(),
        ));
    }
    if request.text.contains('\r') {
        return Err(WorkspaceFilesError::BadParams(
            "write text must use normalized LF line endings".into(),
        ));
    }
    if request.expected_content_hash.is_empty() {
        return Err(WorkspaceFilesError::BadParams(
            "expectedContentHash must not be empty".into(),
        ));
    }
    let source = match request.line_ending {
        WorkspaceWritableLineEnding::Lf => request.text.clone(),
        WorkspaceWritableLineEnding::Crlf => request.text.replace('\n', "\r\n"),
    };
    let mut bytes = Vec::with_capacity(
        source.len() + usize::from(request.encoding == WorkspaceWritableEncoding::Utf8Bom) * 3,
    );
    if request.encoding == WorkspaceWritableEncoding::Utf8Bom {
        bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    bytes.extend_from_slice(source.as_bytes());
    if bytes.len() as u64 > MAX_EDITABLE_FILE_BYTES {
        return Err(WorkspaceFilesError::Unsupported(format!(
            "write exceeds the {MAX_EDITABLE_FILE_BYTES}-byte editable limit"
        )));
    }
    Ok(bytes)
}

fn validate_writable_source(bytes: &[u8]) -> Result<(), WorkspaceFilesError> {
    if bytes.contains(&0) {
        return Err(WorkspaceFilesError::Unsupported(
            "binary files are not writable".into(),
        ));
    }
    let source = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let source = std::str::from_utf8(source).map_err(|_| {
        WorkspaceFilesError::Unsupported("unsupported text encoding is not writable".into())
    })?;
    if detect_line_ending(source.as_bytes()) == WorkspaceLineEnding::Mixed {
        return Err(WorkspaceFilesError::Unsupported(
            "mixed line endings are not writable".into(),
        ));
    }
    Ok(())
}

fn write_file_blocking(
    root: &Path,
    relative: &WorkspaceRelativePath,
    expected_hash: &str,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<WriteWorkspaceFileOutcome, WorkspaceFilesError> {
    let target = root.join(relative.as_path());
    let (metadata, current_bytes) = match current_write_revision(root, relative) {
        Ok(revision) => revision,
        Err(WorkspaceFilesError::NotFound(_)) => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::Deleted,
                None,
                None,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(message)) if message.contains("symlink") => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::Replaced,
                None,
                None,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(message)) if message.contains("editable limit") => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::Changed,
                None,
                None,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(_)) => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::NotRegularFile,
                None,
                None,
            ));
        }
        Err(error) => return Err(error),
    };
    validate_writable_source(&current_bytes)?;
    let current_hash = hash_bytes(&current_bytes);
    if current_hash != expected_hash {
        return Ok(write_conflict(
            WorkspaceFileConflictReason::Changed,
            Some(current_hash),
            metadata.modified().ok().map(chrono::DateTime::from),
        ));
    }
    if cancel.load(Ordering::Acquire) {
        return Err(WorkspaceFilesError::Io("file write cancelled".into()));
    }

    let parent = target
        .parent()
        .ok_or_else(|| WorkspaceFilesError::Io("file has no parent directory".into()))?;
    let temp_path = parent.join(format!(".zeron-save-{}.tmp", uuid::Uuid::new_v4()));
    let mut temp = TempFileGuard::new(temp_path.clone());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    std::fs::set_permissions(&temp_path, metadata.permissions())
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;

    if cancel.load(Ordering::Acquire) {
        return Err(WorkspaceFilesError::Io("file write cancelled".into()));
    }
    let (latest_metadata, latest_bytes) = match current_write_revision(root, relative) {
        Ok(revision) => revision,
        Err(WorkspaceFilesError::NotFound(_)) => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::Deleted,
                None,
                None,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(message)) if message.contains("symlink") => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::Replaced,
                None,
                None,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(message)) if message.contains("editable limit") => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::Changed,
                None,
                None,
            ));
        }
        Err(WorkspaceFilesError::Unsupported(_)) => {
            return Ok(write_conflict(
                WorkspaceFileConflictReason::NotRegularFile,
                None,
                None,
            ));
        }
        Err(error) => return Err(error),
    };
    validate_writable_source(&latest_bytes)?;
    let latest_hash = hash_bytes(&latest_bytes);
    if latest_hash != expected_hash || !same_file_revision(&metadata, &latest_metadata) {
        return Ok(write_conflict(
            WorkspaceFileConflictReason::Changed,
            Some(latest_hash),
            latest_metadata.modified().ok().map(chrono::DateTime::from),
        ));
    }

    atomic_replace(&temp_path, &target)
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    temp.disarm();
    sync_parent_directory(parent);
    let published = checked_file_metadata(root, relative)?;
    let published_bytes =
        std::fs::read(&target).map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    let published_hash = hash_bytes(&published_bytes);
    if published_hash != hash_bytes(bytes) {
        return Err(WorkspaceFilesError::Io(
            "published file verification failed".into(),
        ));
    }
    Ok(WriteWorkspaceFileOutcome::Written {
        file: WorkspaceFileWriteResult {
            path: relative.wire_path(),
            content_hash: published_hash,
            size: published.len(),
            modified_at: published.modified().ok().map(chrono::DateTime::from),
        },
    })
}

fn current_write_revision(
    root: &Path,
    relative: &WorkspaceRelativePath,
) -> Result<(std::fs::Metadata, Vec<u8>), WorkspaceFilesError> {
    let metadata = checked_file_metadata(root, relative)?;
    if metadata.len() > MAX_EDITABLE_FILE_BYTES {
        return Err(WorkspaceFilesError::Unsupported(
            "file exceeds the editable limit".into(),
        ));
    }
    let path = root.join(relative.as_path());
    let bytes = std::fs::read(path).map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    let after = checked_file_metadata(root, relative)?;
    if !same_file_revision(&metadata, &after) || bytes.len() as u64 != after.len() {
        return Err(WorkspaceFilesError::Io(
            "file changed while preparing write; retry".into(),
        ));
    }
    Ok((after, bytes))
}

fn write_conflict(
    reason: WorkspaceFileConflictReason,
    current_content_hash: Option<String>,
    current_modified_at: Option<chrono::DateTime<chrono::Utc>>,
) -> WriteWorkspaceFileOutcome {
    WriteWorkspaceFileOutcome::Conflict {
        reason,
        current_content_hash,
        current_modified_at,
    }
}

struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path.take();
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both pointers reference NUL-terminated buffers for the duration of the call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

fn sync_parent_directory(parent: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
}

fn checked_directory(
    root: &Path,
    directory: &WorkspaceRelativePath,
) -> Result<PathBuf, WorkspaceFilesError> {
    let mut current = root.to_path_buf();
    for component in directory.as_path().components() {
        let Component::Normal(component) = component else {
            return Err(bad_path("invalid directory component"));
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceFilesError::Unsupported(
                "symlink directories cannot be traversed".into(),
            ));
        }
        if !metadata.is_dir() {
            return Err(WorkspaceFilesError::Unsupported(
                "directory path is not a directory".into(),
            ));
        }
    }
    let canonical = std::fs::canonicalize(&current)
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    if !canonical.starts_with(root) {
        return Err(WorkspaceFilesError::Authorization(
            "directory escaped workspace".into(),
        ));
    }
    Ok(canonical)
}

fn entry_group(kind: WorkspaceEntryKind) -> u8 {
    match kind {
        WorkspaceEntryKind::Directory => 0,
        WorkspaceEntryKind::File => 1,
        WorkspaceEntryKind::Symlink => 2,
    }
}

fn contains_git_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(value) if value.to_string_lossy().eq_ignore_ascii_case(".git"))
    })
}

fn path_to_wire(path: &Path) -> Result<String, WorkspaceFilesError> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| WorkspaceFilesError::Unsupported("path is not UTF-8".into())),
            _ => Err(bad_path("invalid relative path")),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

fn directory_fingerprint(entries: &[WorkspaceEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        hasher.update([entry_group(entry.kind)]);
        hasher.update(entry.size.unwrap_or_default().to_le_bytes());
        hasher.update(
            entry
                .modified_at
                .map(|time| time.timestamp_nanos_opt().unwrap_or_default())
                .unwrap_or_default()
                .to_le_bytes(),
        );
    }
    hex(&hasher.finalize())
}

fn encode_cursor(cursor: &DirectoryCursor) -> String {
    let json = serde_json::to_vec(cursor).expect("directory cursor is serializable");
    hex(&json)
}

fn decode_cursor(cursor: &str) -> Result<DirectoryCursor, WorkspaceFilesError> {
    let bytes = decode_hex(cursor)
        .ok_or_else(|| WorkspaceFilesError::BadParams("invalid directory cursor".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| WorkspaceFilesError::BadParams("invalid directory cursor".into()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

fn workspace_search_score(name: &str, path: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let name = name.to_lowercase();
    let path = path.to_lowercase();
    if name == query {
        return Some(10_000);
    }
    if name.starts_with(query) {
        return Some(8_000 - name.len() as i64);
    }
    if let Some(index) = name.find(query) {
        return Some(6_000 - index as i64 - name.len() as i64);
    }
    if let Some(index) = path.find(query) {
        return Some(4_000 - index as i64 - path.len() as i64);
    }
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    let mut gaps = 0i64;
    for character in path.chars() {
        if character == wanted {
            match query_chars.next() {
                Some(next) => wanted = next,
                None => return Some(2_000 - gaps - path.len() as i64),
            }
        } else {
            gaps += 1;
        }
    }
    None
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn relative_paths_preserve_normal_and_unicode_components() {
        let path = WorkspaceRelativePath::file("src/日本語/emoji-🛰️.rs").unwrap();
        assert_eq!(path.as_path(), Path::new("src/日本語/emoji-🛰️.rs"));
        assert_eq!(path.wire_path(), "src/日本語/emoji-🛰️.rs");
        assert_eq!(
            WorkspaceRelativePath::directory("").unwrap().as_path(),
            Path::new("")
        );
    }

    #[test]
    fn relative_paths_reject_unsafe_shapes() {
        for path in [
            "",
            "/tmp/file",
            "./file",
            "src/./file",
            "src/../file",
            "src\\file",
            "src\0file",
            "C:/file",
            "//server/share",
            ".git/config",
            "src/.GIT/config",
        ] {
            assert!(
                WorkspaceRelativePath::file(path).is_err(),
                "unsafe path accepted: {path:?}"
            );
        }
    }

    #[test]
    fn relative_paths_reject_drive_letters_and_ntfs_alternate_data_streams() {
        for path in [
            "C:/file.txt",
            "C:file.txt",
            "file.txt:stream",
            "dir/file.txt:stream",
            "dir:stream/file.txt",
            ":stream",
        ] {
            assert!(
                WorkspaceRelativePath::file(path).is_err(),
                "unsafe file path accepted: {path:?}"
            );
            assert!(
                WorkspaceRelativePath::directory(path).is_err(),
                "unsafe directory path accepted: {path:?}"
            );
        }
    }

    #[test]
    fn list_orders_directories_pages_and_detects_stale_cursors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Zoo")).unwrap();
        std::fs::create_dir(root.path().join("alpha")).unwrap();
        for index in 0..501 {
            std::fs::write(root.path().join(format!("file-{index:03}.txt")), b"x").unwrap();
        }
        let root = std::fs::canonicalize(root.path()).unwrap();
        let first = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            None,
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(first.entries.len(), DIRECTORY_PAGE_SIZE);
        assert_eq!(first.entries[0].path, "alpha");
        assert_eq!(first.entries[1].path, "Zoo");
        let cursor = first.next_cursor.unwrap();

        let second = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            Some(&cursor),
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(second.entries.len(), 3);
        assert!(second.next_cursor.is_none());

        std::fs::write(root.join("new.txt"), b"new").unwrap();
        let stale = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            Some(&cursor),
            &no_cancel(),
        )
        .unwrap_err();
        assert!(stale.to_string().contains("restart listing"));
    }

    #[test]
    fn list_honors_ignore_rules_but_never_exposes_git() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.path().join(".hidden"), b"hidden").unwrap();
        std::fs::write(root.path().join("ignored.txt"), b"ignored").unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/config"), b"secret").unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();

        let filtered = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            None,
            &no_cancel(),
        )
        .unwrap();
        assert!(filtered.entries.iter().any(|entry| entry.path == ".hidden"));
        assert!(
            !filtered
                .entries
                .iter()
                .any(|entry| entry.path == "ignored.txt")
        );

        let all = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            true,
            None,
            &no_cancel(),
        )
        .unwrap();
        assert!(
            all.entries
                .iter()
                .any(|entry| entry.path == "ignored.txt" && entry.ignored)
        );
        assert!(
            !all.entries
                .iter()
                .any(|entry| entry.path == ".git" || entry.path.starts_with(".git/"))
        );
    }

    #[test]
    fn search_ranks_filename_and_nested_path_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/deep")).unwrap();
        std::fs::write(root.path().join("src/deep/config.rs"), b"").unwrap();
        std::fs::write(root.path().join("src/configuration.rs"), b"").unwrap();
        std::fs::write(root.path().join("README.md"), b"").unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();

        let matches = search_workspace_blocking(&root, "config", false, 200, &no_cancel()).unwrap();
        assert_eq!(matches[0].path, "src/deep/config.rs");
        assert!(
            matches
                .iter()
                .any(|entry| entry.path == "src/configuration.rs")
        );
        assert!(matches.len() <= MAX_SEARCH_RESULTS);
    }

    #[test]
    fn search_rejects_empty_and_whitespace_queries() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();

        for query in ["", "   ", "\n\t"] {
            let error =
                search_workspace_blocking(&root, query, false, 200, &no_cancel()).unwrap_err();
            assert!(error.to_string().contains("query must not be empty"));
        }
    }

    #[test]
    fn search_keeps_only_the_best_matches_while_walking() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "aaa-query-notes.txt",
            "bbb-query-notes.txt",
            "ccc-query-notes.txt",
            "query",
            "query-reference.txt",
        ] {
            std::fs::write(root.path().join(name), b"").unwrap();
        }
        let root = std::fs::canonicalize(root.path()).unwrap();

        let matches = search_workspace_blocking(&root, "query", false, 2, &no_cancel()).unwrap();

        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "query");
        assert_eq!(matches[1].path, "query-reference.txt");
    }

    fn read_fixture(bytes: &[u8]) -> WorkspaceFileText {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.txt"), bytes).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        read_file_blocking(
            &canonical,
            &WorkspaceRelativePath::file("file.txt").unwrap(),
            &no_cancel(),
        )
        .unwrap()
    }

    #[test]
    fn read_classifies_utf8_bom_and_line_endings() {
        let lf = read_fixture("hello\n世界\n".as_bytes());
        assert_eq!(lf.encoding, WorkspaceTextEncoding::Utf8);
        assert_eq!(lf.line_ending, Some(WorkspaceLineEnding::Lf));
        assert_eq!(lf.text.as_deref(), Some("hello\n世界\n"));

        let crlf = read_fixture(b"first\r\nsecond\r\n");
        assert_eq!(crlf.line_ending, Some(WorkspaceLineEnding::Crlf));
        assert_eq!(crlf.text.as_deref(), Some("first\nsecond\n"));

        let bom = read_fixture(b"\xef\xbb\xbfhello\r\n");
        assert_eq!(bom.encoding, WorkspaceTextEncoding::Utf8Bom);
        assert_eq!(bom.text.as_deref(), Some("hello\n"));

        let mixed = read_fixture(b"first\r\nsecond\n");
        assert_eq!(mixed.line_ending, Some(WorkspaceLineEnding::Mixed));
        assert_eq!(
            mixed.read_only_reason,
            Some(WorkspaceReadOnlyReason::MixedLineEndings)
        );
    }

    #[test]
    fn read_hashes_original_bytes_and_rejects_lossy_text() {
        let lf = read_fixture(b"hello\n");
        let crlf = read_fixture(b"hello\r\n");
        assert_ne!(lf.content_hash, crlf.content_hash);
        assert_eq!(
            lf.content_hash.as_deref(),
            Some(hash_bytes(b"hello\n").as_str())
        );

        let binary = read_fixture(b"hello\0world");
        assert_eq!(binary.encoding, WorkspaceTextEncoding::Binary);
        assert!(binary.text.is_none());
        let unsupported = read_fixture(&[0xff, 0xfe, 0xfd]);
        assert_eq!(unsupported.encoding, WorkspaceTextEncoding::Unsupported);
        assert!(unsupported.text.is_none());
    }

    #[test]
    fn read_enforces_editable_and_preview_limits() {
        let editable = read_fixture(&vec![b'a'; MAX_EDITABLE_FILE_BYTES as usize]);
        assert!(editable.read_only_reason.is_none());
        let preview = read_fixture(&vec![b'a'; MAX_EDITABLE_FILE_BYTES as usize + 1]);
        assert_eq!(
            preview.read_only_reason,
            Some(WorkspaceReadOnlyReason::TooLarge)
        );
        assert!(preview.text.is_some());

        let root = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(root.path().join("large.txt")).unwrap();
        file.set_len(MAX_PREVIEW_FILE_BYTES + 1).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let large = read_file_blocking(
            &canonical,
            &WorkspaceRelativePath::file("large.txt").unwrap(),
            &no_cancel(),
        )
        .unwrap();
        assert!(large.truncated);
        assert!(large.text.is_none());
        assert!(large.content_hash.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_does_not_follow_file_or_parent_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            root.path().join("file.txt"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("dir")).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();

        let file = read_file_blocking(
            &canonical,
            &WorkspaceRelativePath::file("file.txt").unwrap(),
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(
            file.read_only_reason,
            Some(WorkspaceReadOnlyReason::Symlink)
        );
        assert!(
            read_file_blocking(
                &canonical,
                &WorkspaceRelativePath::file("dir/secret.txt").unwrap(),
                &no_cancel(),
            )
            .is_err()
        );
    }

    fn write_request(
        text: &str,
        expected_content_hash: String,
        encoding: WorkspaceWritableEncoding,
        line_ending: WorkspaceWritableLineEnding,
    ) -> WriteWorkspaceFileRequest {
        WriteWorkspaceFileRequest {
            expected_checkout_id: "checkout-test".into(),
            target: WorkspaceTarget {
                chat_id: Some("chat".into()),
                space_id: None,
                checkout_path: None,
            },
            path: "file.txt".into(),
            text: text.into(),
            expected_content_hash,
            encoding,
            line_ending,
        }
    }

    #[test]
    fn write_preserves_requested_encoding_line_endings_and_permissions() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, b"old\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let relative = WorkspaceRelativePath::file("file.txt").unwrap();
        let request = write_request(
            "first\nsecond\n",
            hash_bytes(b"old\n"),
            WorkspaceWritableEncoding::Utf8Bom,
            WorkspaceWritableLineEnding::Crlf,
        );
        let bytes = encode_write_text(&request).unwrap();
        let outcome = write_file_blocking(
            &canonical,
            &relative,
            &request.expected_content_hash,
            &bytes,
            &no_cancel(),
        )
        .unwrap();
        let expected = b"\xef\xbb\xbffirst\r\nsecond\r\n";
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let WriteWorkspaceFileOutcome::Written { file } = outcome else {
            panic!("expected written outcome");
        };
        assert_eq!(file.content_hash, hash_bytes(expected));
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn write_conflicts_leave_the_current_file_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, b"current\n").unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let relative = WorkspaceRelativePath::file("file.txt").unwrap();
        let request = write_request(
            "replacement\n",
            hash_bytes(b"stale\n"),
            WorkspaceWritableEncoding::Utf8,
            WorkspaceWritableLineEnding::Lf,
        );
        let bytes = encode_write_text(&request).unwrap();
        let outcome = write_file_blocking(
            &canonical,
            &relative,
            &request.expected_content_hash,
            &bytes,
            &no_cancel(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            WriteWorkspaceFileOutcome::Conflict {
                reason: WorkspaceFileConflictReason::Changed,
                ..
            }
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"current\n");
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".zeron-save-")
        }));
    }

    #[test]
    fn write_requires_normalized_text_and_enforces_size_limit() {
        let cr = write_request(
            "not\r\nnormalized",
            "hash".into(),
            WorkspaceWritableEncoding::Utf8,
            WorkspaceWritableLineEnding::Lf,
        );
        assert!(encode_write_text(&cr).is_err());
        let nul = write_request(
            "not\0text",
            "hash".into(),
            WorkspaceWritableEncoding::Utf8,
            WorkspaceWritableLineEnding::Lf,
        );
        assert!(encode_write_text(&nul).is_err());
        let large = write_request(
            &"x".repeat(MAX_EDITABLE_FILE_BYTES as usize + 1),
            "hash".into(),
            WorkspaceWritableEncoding::Utf8,
            WorkspaceWritableLineEnding::Lf,
        );
        assert!(encode_write_text(&large).is_err());
    }

    #[test]
    fn write_rejects_binary_and_mixed_source_files() {
        for source in [
            b"binary\0source".as_slice(),
            b"mixed\r\nsource\n".as_slice(),
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("file.txt"), source).unwrap();
            let canonical = std::fs::canonicalize(root.path()).unwrap();
            let result = write_file_blocking(
                &canonical,
                &WorkspaceRelativePath::file("file.txt").unwrap(),
                &hash_bytes(source),
                b"replacement\n",
                &no_cancel(),
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read(root.path().join("file.txt")).unwrap(), source);
        }
    }

    #[test]
    fn write_deleted_file_returns_typed_conflict_without_creating_it() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let relative = WorkspaceRelativePath::file("file.txt").unwrap();
        let outcome =
            write_file_blocking(&canonical, &relative, "old-hash", b"new\n", &no_cancel()).unwrap();
        assert!(matches!(
            outcome,
            WriteWorkspaceFileOutcome::Conflict {
                reason: WorkspaceFileConflictReason::Deleted,
                ..
            }
        ));
        assert!(!root.path().join("file.txt").exists());
    }

    #[test]
    fn watch_normalizes_renames_deduplicates_and_filters_git() {
        use notify::EventKind;
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

        let root = Path::new("/workspace");
        let events = vec![
            Ok(notify::Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.join("src/lib.rs"))),
            Ok(notify::Event::new(EventKind::Remove(RemoveKind::File))
                .add_path(root.join("src/lib.rs"))),
            Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(root.join(".git/index"))),
            Ok(
                notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(root.join("old.rs"))
                    .add_path(root.join("new.rs")),
            ),
            Ok(
                notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(root.join(".zeron-save-dead.tmp"))
                    .add_path(root.join("saved.rs")),
            ),
        ];
        let (resync, changes) = normalize_watch_events(root, events);
        assert!(!resync);
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().any(|change| {
            change.path == "src/lib.rs" && change.kind == WorkspaceFileChangeKind::Removed
        }));
        assert!(changes.iter().any(|change| {
            change.path == "new.rs"
                && change.old_path.as_deref() == Some("old.rs")
                && change.kind == WorkspaceFileChangeKind::Renamed
        }));
        assert!(changes.iter().any(|change| {
            change.path == "saved.rs"
                && change.old_path.is_none()
                && change.kind == WorkspaceFileChangeKind::Modified
        }));
    }

    #[test]
    fn watch_preserves_the_final_state_of_remove_and_create_sequences() {
        use notify::EventKind;
        use notify::event::{CreateKind, RemoveKind};

        let root = Path::new("/workspace");
        let recreated = vec![
            Ok(notify::Event::new(EventKind::Remove(RemoveKind::File))
                .add_path(root.join("recreated.rs"))),
            Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(root.join("recreated.rs"))),
        ];
        let (_, changes) = normalize_watch_events(root, recreated);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "recreated.rs");
        assert_eq!(changes[0].kind, WorkspaceFileChangeKind::Created);

        let removed = vec![
            Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(root.join("removed.rs"))),
            Ok(notify::Event::new(EventKind::Remove(RemoveKind::File))
                .add_path(root.join("removed.rs"))),
        ];
        let (_, changes) = normalize_watch_events(root, removed);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "removed.rs");
        assert_eq!(changes[0].kind, WorkspaceFileChangeKind::Removed);
    }

    #[tokio::test]
    async fn watch_starts_with_resync_and_shares_one_checkout_entry() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let watch = CheckoutWatch::start(
            "checkout".into(),
            canonical,
            false,
            CancellationToken::new(),
        );
        let owner = Weak::<WorkspaceFilesInner>::new();
        let mut first = watch.subscribe(owner.clone());
        let second = watch.subscribe(owner);
        assert_eq!(watch.subscribers.load(Ordering::Acquire), 2);
        let baseline = first.recv().await.unwrap();
        assert!(baseline.resync_required);
        assert!(baseline.changes.is_empty());
        drop(second);
        assert_eq!(watch.subscribers.load(Ordering::Acquire), 1);
        assert!(!watch.cancel.is_cancelled());
        drop(first);
        assert!(watch.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn watch_streams_native_file_changes_and_stops_on_cancel() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let watch = CheckoutWatch::start(
            "checkout".into(),
            canonical.clone(),
            false,
            CancellationToken::new(),
        );
        let mut subscription = watch.subscribe(Weak::<WorkspaceFilesInner>::new());
        subscription.recv().await.unwrap();
        std::fs::write(canonical.join("created.txt"), b"hello").unwrap();
        let batch = tokio::time::timeout(Duration::from_secs(3), subscription.recv())
            .await
            .expect("watch event timeout")
            .expect("watch closed");
        assert!(
            batch
                .changes
                .iter()
                .any(|change| change.path == "created.txt")
        );
        watch.cancel.cancel();
        assert!(subscription.recv().await.is_none());
    }

    #[tokio::test]
    async fn watch_reconciles_native_remove_and_recreate_bursts_promptly() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let path = canonical.join("recreated.txt");
        std::fs::write(&path, b"before").unwrap();
        let watch = CheckoutWatch::start(
            "checkout".into(),
            canonical,
            false,
            CancellationToken::new(),
        );
        let mut subscription = watch.subscribe(Weak::<WorkspaceFilesInner>::new());
        subscription.recv().await.unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"after").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut recovered = false;
        while tokio::time::Instant::now() < deadline {
            let batch = tokio::time::timeout_at(deadline, subscription.recv())
                .await
                .expect("watch event timeout")
                .expect("watch closed");
            if batch.changes.iter().any(|change| {
                change.path == "recreated.txt"
                    && matches!(
                        change.kind,
                        WorkspaceFileChangeKind::Created | WorkspaceFileChangeKind::Modified
                    )
            }) {
                recovered = true;
                break;
            }
        }

        assert!(recovered, "recreated file never reached its final state");
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
    }

    #[tokio::test]
    async fn watch_reports_native_atomic_replacements_promptly() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let path = canonical.join("replaced.txt");
        let replacement = canonical.join("replacement.tmp");
        std::fs::write(&path, b"before").unwrap();
        let watch = CheckoutWatch::start(
            "checkout".into(),
            canonical,
            false,
            CancellationToken::new(),
        );
        let mut subscription = watch.subscribe(Weak::<WorkspaceFilesInner>::new());
        subscription.recv().await.unwrap();

        std::fs::write(&replacement, b"after").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut reported = false;
        while tokio::time::Instant::now() < deadline {
            let batch = tokio::time::timeout_at(deadline, subscription.recv())
                .await
                .expect("watch event timeout")
                .expect("watch closed");
            if batch
                .changes
                .iter()
                .any(|change| change.path == "replaced.txt")
            {
                reported = true;
                break;
            }
        }

        assert!(reported, "atomic replacement was not reported");
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
    }

    #[tokio::test]
    async fn watch_publishes_before_a_continuous_native_burst_finishes() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let path = canonical.join("busy.txt");
        std::fs::write(&path, b"0").unwrap();
        let watch = CheckoutWatch::start(
            "checkout".into(),
            canonical,
            false,
            CancellationToken::new(),
        );
        let mut subscription = watch.subscribe(Weak::<WorkspaceFilesInner>::new());
        subscription.recv().await.unwrap();

        let producer = tokio::spawn(async move {
            for value in 1..=50 {
                std::fs::write(&path, value.to_string()).unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let batch = tokio::time::timeout(Duration::from_millis(1_800), subscription.recv())
            .await
            .expect("continuous burst postponed publication")
            .expect("watch closed");

        assert!(batch.changes.iter().any(|change| change.path == "busy.txt"));
        assert!(!producer.is_finished());
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn watch_repair_only_mode_keeps_the_stream_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let watch =
            CheckoutWatch::start("checkout".into(), canonical, true, CancellationToken::new());
        assert!(lock(&watch.watcher).is_none());
        let mut subscription = watch.subscribe(Weak::<WorkspaceFilesInner>::new());
        assert!(subscription.recv().await.unwrap().resync_required);
        watch.cancel.cancel();
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use zeron_proto::{ReadWorkspaceImageRequest, WORKSPACE_IMAGE_CHUNK_BYTES, WorkspaceTarget};
    fn request() -> ReadWorkspaceImageRequest {
        ReadWorkspaceImageRequest {
            target: WorkspaceTarget {
                chat_id: Some("chat".into()),
                space_id: None,
                checkout_path: None,
            },
            path: "image.png".into(),
            expected_checkout_id: "checkout".into(),
            offset: 0,
            expected_content_hash: None,
        }
    }
    #[test]
    fn workspace_image_chunks_are_bounded_and_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("image.png"),
            vec![1; WORKSPACE_IMAGE_CHUNK_BYTES + 1],
        )
        .unwrap();
        let path = WorkspaceRelativePath::file("image.png").unwrap();
        let mut request = request();
        let first = read_image_blocking(&root, &path, &request).unwrap();
        assert!(!first.done);
        assert_eq!(first.next_offset, WORKSPACE_IMAGE_CHUNK_BYTES);
        assert!(first.data.len() < 1024 * 1024);
        request.offset = first.next_offset;
        assert!(read_image_blocking(&root, &path, &request).is_err());
        request.expected_content_hash = Some(first.content_hash);
        assert!(read_image_blocking(&root, &path, &request).unwrap().done);
        std::fs::write(
            root.join("image.png"),
            vec![2; WORKSPACE_IMAGE_CHUNK_BYTES + 1],
        )
        .unwrap();
        assert!(read_image_blocking(&root, &path, &request).is_err());
        request.expected_content_hash = None;
        request.offset = usize::MAX;
        assert!(read_image_blocking(&root, &path, &request).is_err());
    }
    #[test]
    fn workspace_images_reject_large_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = std::fs::File::create(root.join("image.png")).unwrap();
        file.set_len(zeron_proto::MAX_WORKSPACE_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(
            read_image_blocking(
                &root,
                &WorkspaceRelativePath::file("image.png").unwrap(),
                &request()
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.join("link.png")).unwrap();
            assert!(
                read_image_blocking(
                    &root,
                    &WorkspaceRelativePath::file("link.png").unwrap(),
                    &request()
                )
                .is_err()
            );
        }
    }
}
