//! Demand-driven checkout change-request cache and polling streams.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use zeron_proto::CheckoutChangeRequestStatus;

use crate::repos::{CheckoutIdentity, Repos};
use crate::source_control::{
    BranchHeadContext, ChangeRequestError, ChangeRequestResolver, CheckoutChangeRequestLookup,
    CheckoutSourceContext, parse_git_remote,
};

const WITH_CHANGE_REQUEST_TTL: Duration = Duration::from_secs(2 * 60);
const WITHOUT_CHANGE_REQUEST_TTL: Duration = Duration::from_secs(45);
const FAILURE_INITIAL_BACKOFF: Duration = Duration::from_secs(20);
const FAILURE_MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const CONTEXT_POLL_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeRequestCacheKey {
    pub checkout_id: String,
    pub branch: String,
    pub upstream_ref: String,
    pub remote: String,
}

#[derive(Debug, Clone, Copy)]
struct Timing {
    with_change_request_ttl: Duration,
    without_change_request_ttl: Duration,
    failure_initial_backoff: Duration,
    failure_max_backoff: Duration,
    context_poll_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            with_change_request_ttl: WITH_CHANGE_REQUEST_TTL,
            without_change_request_ttl: WITHOUT_CHANGE_REQUEST_TTL,
            failure_initial_backoff: FAILURE_INITIAL_BACKOFF,
            failure_max_backoff: FAILURE_MAX_BACKOFF,
            context_poll_interval: CONTEXT_POLL_INTERVAL,
        }
    }
}

struct CacheEntry {
    state: AsyncMutex<CacheState>,
    demand: AtomicUsize,
}

struct CacheState {
    last_success: Option<CheckoutChangeRequestStatus>,
    next_refresh: Instant,
    failures: u32,
    last_error: Option<ChangeRequestError>,
}

struct Inner {
    repos: Repos,
    device_id: String,
    lookup: Arc<dyn CheckoutChangeRequestLookup>,
    entries: Mutex<HashMap<ChangeRequestCacheKey, Arc<CacheEntry>>>,
    timing: Timing,
    shutdown: CancellationToken,
}

/// Shared host-side cache. It owns no polling task: a stream drives refreshes only
/// while it is actively polled, so dropping the last RPC subscription stops work.
#[derive(Clone)]
pub struct CheckoutChangeRequests {
    inner: Arc<Inner>,
}

impl CheckoutChangeRequests {
    pub fn start(repos: Repos, device_id: &str) -> Self {
        Self::new(repos, device_id, Arc::new(ChangeRequestResolver::new()))
    }

    pub fn new(
        repos: Repos,
        device_id: &str,
        lookup: Arc<dyn CheckoutChangeRequestLookup>,
    ) -> Self {
        Self::with_timing(repos, device_id, lookup, Timing::default())
    }

    fn with_timing(
        repos: Repos,
        device_id: &str,
        lookup: Arc<dyn CheckoutChangeRequestLookup>,
        timing: Timing,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                repos,
                device_id: device_id.to_owned(),
                lookup,
                entries: Mutex::new(HashMap::new()),
                timing,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub async fn watch(
        &self,
        cwd: &Path,
    ) -> Result<BoxStream<'static, CheckoutChangeRequestStatus>, ChangeRequestError> {
        self.watch_for_branch(cwd, None).await
    }

    /// Resolve a PR for the conversation-owned branch while using `cwd` only
    /// for repository/remote identity. This keeps a later checkout switch from
    /// changing which PR an older conversation displays.
    pub async fn watch_for_branch(
        &self,
        cwd: &Path,
        branch: Option<&str>,
    ) -> Result<BoxStream<'static, CheckoutChangeRequestStatus>, ChangeRequestError> {
        let identity = self
            .inner
            .repos
            .checkout_identity(cwd)
            .await
            .map_err(|_| ChangeRequestError::RepositoryUnavailable)?;
        Ok(self.watch_checkout_for_branch(cwd.to_owned(), identity, branch.map(str::to_owned)))
    }

    #[cfg(test)]
    fn watch_checkout(
        &self,
        cwd: PathBuf,
        identity: CheckoutIdentity,
    ) -> BoxStream<'static, CheckoutChangeRequestStatus> {
        self.watch_checkout_for_branch(cwd, identity, None)
    }

    fn watch_checkout_for_branch(
        &self,
        cwd: PathBuf,
        identity: CheckoutIdentity,
        requested_branch: Option<String>,
    ) -> BoxStream<'static, CheckoutChangeRequestStatus> {
        let state = SubscriptionState {
            service: self.clone(),
            cwd,
            identity,
            requested_branch,
            lease: None,
            last_emitted: None,
        };
        futures::stream::unfold(state, |mut state| async move {
            loop {
                if state.service.inner.shutdown.is_cancelled() {
                    return None;
                }
                let mut source = match state
                    .service
                    .inner
                    .lookup
                    .inspect_checkout(&state.cwd)
                    .await
                {
                    Ok(source) => source,
                    Err(error) => {
                        tracing::debug!(%error, "change request checkout inspection unavailable");
                        if !state
                            .sleep(state.service.inner.timing.context_poll_interval)
                            .await
                        {
                            return None;
                        }
                        continue;
                    }
                };
                if let Some(branch) = state.requested_branch.as_deref()
                    && source.branch.local_branch != branch
                {
                    source.branch = BranchHeadContext::resolve(
                        branch,
                        None,
                        source.branch.remote_name.as_deref(),
                        source.branch.remote_url.as_deref(),
                    );
                }
                let key = cache_key(&state.identity.id, &source);
                if state.lease.as_ref().map(|lease| &lease.key) != Some(&key) {
                    state.lease = Some(state.service.acquire(key.clone()));
                }
                let (snapshot, next_refresh) = state.service.refresh(&key, &source).await;
                if let Some(snapshot) = snapshot {
                    let semantic = SemanticStatus::from(&snapshot);
                    if state.last_emitted.as_ref() != Some(&semantic) {
                        state.last_emitted = Some(semantic);
                        return Some((snapshot, state));
                    }
                }

                let until_refresh = next_refresh.saturating_duration_since(Instant::now());
                let delay = until_refresh.min(state.service.inner.timing.context_poll_interval);
                if !state.sleep(delay).await {
                    return None;
                }
            }
        })
        .boxed()
    }

    fn acquire(&self, key: ChangeRequestCacheKey) -> DemandLease {
        let entry = {
            let mut entries = self
                .inner
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let entry = entries
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(CacheEntry {
                        state: AsyncMutex::new(CacheState {
                            last_success: None,
                            next_refresh: Instant::now(),
                            failures: 0,
                            last_error: None,
                        }),
                        demand: AtomicUsize::new(0),
                    })
                })
                .clone();
            // Keep the demand transition under the entries lock. The final
            // lease drop uses that same lock before evicting, so a new watcher
            // cannot resurrect an entry after it was removed from the map.
            entry.demand.fetch_add(1, Ordering::AcqRel);
            entry
        };
        DemandLease {
            key,
            entry,
            inner: Arc::downgrade(&self.inner),
        }
    }

    async fn refresh(
        &self,
        key: &ChangeRequestCacheKey,
        source: &CheckoutSourceContext,
    ) -> (Option<CheckoutChangeRequestStatus>, Instant) {
        let entry = {
            let entries = self
                .inner
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            entries.get(key).cloned().expect("cache entry acquired")
        };
        let mut state = entry.state.lock().await;
        let now = Instant::now();
        if now < state.next_refresh {
            return (state.last_success.clone(), state.next_refresh);
        }

        match self.inner.lookup.resolve_github_source(source).await {
            Ok(change_request) => {
                let ttl = if change_request.is_some() {
                    self.inner.timing.with_change_request_ttl
                } else {
                    self.inner.timing.without_change_request_ttl
                };
                let snapshot = CheckoutChangeRequestStatus {
                    checkout_id: key.checkout_id.clone(),
                    device_id: self.inner.device_id.clone(),
                    cwd: source.checkout_root.to_string_lossy().into_owned(),
                    branch: source.branch.local_branch.clone(),
                    change_request,
                    updated_at: chrono::Utc::now(),
                };
                state.last_success = Some(snapshot.clone());
                state.next_refresh = Instant::now() + ttl;
                state.failures = 0;
                state.last_error = None;
                (Some(snapshot), state.next_refresh)
            }
            Err(error) => {
                state.failures = state.failures.saturating_add(1);
                let delay = failure_backoff(
                    self.inner.timing.failure_initial_backoff,
                    self.inner.timing.failure_max_backoff,
                    state.failures,
                );
                state.next_refresh = Instant::now() + delay;
                if state.last_error != Some(error) {
                    tracing::warn!(%error, "change request refresh failed; retaining last success");
                    state.last_error = Some(error);
                }
                (state.last_success.clone(), state.next_refresh)
            }
        }
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }
}

struct DemandLease {
    key: ChangeRequestCacheKey,
    entry: Arc<CacheEntry>,
    inner: Weak<Inner>,
}

impl Drop for DemandLease {
    fn drop(&mut self) {
        if self.entry.demand.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut entries = inner.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let is_cached_entry = entries
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry));
        if is_cached_entry && self.entry.demand.load(Ordering::Acquire) == 0 {
            entries.remove(&self.key);
        }
    }
}

struct SubscriptionState {
    service: CheckoutChangeRequests,
    cwd: PathBuf,
    identity: CheckoutIdentity,
    requested_branch: Option<String>,
    lease: Option<DemandLease>,
    last_emitted: Option<SemanticStatus>,
}

impl SubscriptionState {
    async fn sleep(&self, duration: Duration) -> bool {
        tokio::select! {
            _ = self.service.inner.shutdown.cancelled() => false,
            _ = tokio::time::sleep(duration) => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticStatus {
    checkout_id: String,
    device_id: String,
    cwd: String,
    branch: String,
    change_request: Option<zeron_proto::ChangeRequestSummary>,
}

impl From<&CheckoutChangeRequestStatus> for SemanticStatus {
    fn from(status: &CheckoutChangeRequestStatus) -> Self {
        Self {
            checkout_id: status.checkout_id.clone(),
            device_id: status.device_id.clone(),
            cwd: status.cwd.clone(),
            branch: status.branch.clone(),
            change_request: status.change_request.clone(),
        }
    }
}

fn cache_key(checkout_id: &str, source: &CheckoutSourceContext) -> ChangeRequestCacheKey {
    let remote = source
        .branch
        .remote_url
        .as_deref()
        .and_then(parse_git_remote)
        .map(|remote| {
            format!(
                "{}/{}/{}",
                remote.host.to_ascii_lowercase(),
                remote.owner.to_ascii_lowercase(),
                remote.repository.to_ascii_lowercase()
            )
        })
        .unwrap_or_default();
    ChangeRequestCacheKey {
        checkout_id: checkout_id.to_owned(),
        branch: source.branch.local_branch.clone(),
        upstream_ref: source.branch.upstream_ref.clone().unwrap_or_default(),
        remote,
    }
}

fn failure_backoff(initial: Duration, maximum: Duration, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(31);
    initial.saturating_mul(1_u32 << exponent).min(maximum)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use futures::StreamExt;
    use zeron_proto::{ChangeRequestState, ChangeRequestSummary};

    use super::*;
    use crate::source_control::BranchHeadContext;

    struct FakeLookup {
        source: Mutex<CheckoutSourceContext>,
        results: Mutex<VecDeque<Result<Option<ChangeRequestSummary>, ChangeRequestError>>>,
        resolves: AtomicUsize,
    }

    impl FakeLookup {
        fn new(
            source: CheckoutSourceContext,
            results: impl IntoIterator<Item = Result<Option<ChangeRequestSummary>, ChangeRequestError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                source: Mutex::new(source),
                results: Mutex::new(results.into_iter().collect()),
                resolves: AtomicUsize::new(0),
            })
        }

        fn set_source(&self, source: CheckoutSourceContext) {
            *self.source.lock().unwrap() = source;
        }

        fn resolve_count(&self) -> usize {
            self.resolves.load(Ordering::Acquire)
        }
    }

    #[async_trait]
    impl CheckoutChangeRequestLookup for FakeLookup {
        async fn inspect_checkout(
            &self,
            _cwd: &Path,
        ) -> Result<CheckoutSourceContext, ChangeRequestError> {
            Ok(self.source.lock().unwrap().clone())
        }

        async fn resolve_github_source(
            &self,
            _source: &CheckoutSourceContext,
        ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
            self.resolves.fetch_add(1, Ordering::AcqRel);
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .expect("missing fake lookup result")
        }
    }

    fn source(branch: &str) -> CheckoutSourceContext {
        CheckoutSourceContext {
            checkout_root: PathBuf::from("/checkout"),
            branch: BranchHeadContext::resolve(
                branch,
                Some(&format!("origin/{branch}")),
                Some("origin"),
                Some("git@github.com:acme/zeron.git"),
            ),
            default_branch: Some("main".into()),
        }
    }

    fn pull_request(number: u64) -> ChangeRequestSummary {
        ChangeRequestSummary {
            provider: "github".into(),
            number,
            title: format!("Pull request {number}"),
            url: format!("https://github.com/acme/zeron/pull/{number}"),
            state: ChangeRequestState::Open,
            base_ref: "main".into(),
            head_ref: "feature/status".into(),
        }
    }

    fn identity() -> CheckoutIdentity {
        CheckoutIdentity {
            id: "checkout-1".into(),
            root: PathBuf::from("/checkout"),
            git_dir: PathBuf::from("/checkout/.git"),
        }
    }

    fn service(lookup: Arc<FakeLookup>, timing: Timing) -> CheckoutChangeRequests {
        let temp = tempfile::tempdir().unwrap();
        CheckoutChangeRequests::with_timing(
            Repos::new(temp.path(), "device-1"),
            "device-1",
            lookup,
            timing,
        )
    }

    fn fast_timing() -> Timing {
        Timing {
            with_change_request_ttl: Duration::from_millis(15),
            without_change_request_ttl: Duration::from_millis(10),
            failure_initial_backoff: Duration::from_secs(1),
            failure_max_backoff: Duration::from_secs(2),
            context_poll_interval: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn requested_conversation_branch_overrides_live_checkout_branch() {
        let lookup = FakeLookup::new(source("main"), [Ok(Some(pull_request(90)))]);
        let service = service(lookup, Timing::default());
        let mut stream = service.watch_checkout_for_branch(
            PathBuf::from("/checkout"),
            identity(),
            Some("feature/sidebar".into()),
        );
        let snapshot = stream.next().await.expect("opening snapshot");
        assert_eq!(snapshot.branch, "feature/sidebar");
    }

    #[tokio::test]
    async fn two_subscribers_share_one_provider_query() {
        let lookup = FakeLookup::new(source("feature/status"), [Ok(Some(pull_request(90)))]);
        let service = service(lookup.clone(), Timing::default());
        let mut first = service.watch_checkout(PathBuf::from("/chat-a"), identity());
        let mut second = service.watch_checkout(PathBuf::from("/chat-b"), identity());

        let (first, second) = tokio::join!(first.next(), second.next());

        assert_eq!(first.unwrap().change_request.unwrap().number, 90);
        assert_eq!(second.unwrap().change_request.unwrap().number, 90);
        assert_eq!(lookup.resolve_count(), 1);
    }

    #[tokio::test]
    async fn branch_change_moves_to_a_new_cache_key() {
        let lookup = FakeLookup::new(
            source("feature/one"),
            [Ok(Some(pull_request(1))), Ok(Some(pull_request(2)))],
        );
        let service = service(lookup.clone(), fast_timing());
        let mut stream = service.watch_checkout(PathBuf::from("/checkout"), identity());
        let first = stream.next().await.unwrap();
        lookup.set_source(source("feature/two"));

        let second = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first.branch, "feature/one");
        assert_eq!(second.branch, "feature/two");
        assert_eq!(second.change_request.unwrap().number, 2);
        assert_eq!(lookup.resolve_count(), 2);
    }

    #[tokio::test]
    async fn successful_none_clears_a_previous_pull_request() {
        let lookup = FakeLookup::new(
            source("feature/status"),
            [Ok(Some(pull_request(90))), Ok(None)],
        );
        let service = service(lookup, fast_timing());
        let mut stream = service.watch_checkout(PathBuf::from("/checkout"), identity());
        assert!(stream.next().await.unwrap().change_request.is_some());

        let cleared = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(cleared.change_request, None);
    }

    #[tokio::test]
    async fn unchanged_success_is_not_reemitted() {
        let lookup = FakeLookup::new(
            source("feature/status"),
            [
                Ok(Some(pull_request(90))),
                Ok(Some(pull_request(90))),
                Ok(Some(pull_request(91))),
            ],
        );
        let service = service(lookup.clone(), fast_timing());
        let mut stream = service.watch_checkout(PathBuf::from("/checkout"), identity());
        assert_eq!(
            stream.next().await.unwrap().change_request.unwrap().number,
            90
        );

        let changed = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(changed.change_request.unwrap().number, 91);
        assert_eq!(lookup.resolve_count(), 3);
    }

    #[tokio::test]
    async fn failure_keeps_the_last_success() {
        let lookup = FakeLookup::new(
            source("feature/status"),
            [
                Ok(Some(pull_request(90))),
                Err(ChangeRequestError::RateLimited),
            ],
        );
        let service = service(lookup.clone(), fast_timing());
        let mut stream = service.watch_checkout(PathBuf::from("/checkout"), identity());
        let first = stream.next().await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err()
        );
        assert_eq!(lookup.resolve_count(), 2);

        let mut later = service.watch_checkout(PathBuf::from("/checkout"), identity());
        let cached = later.next().await.unwrap();
        assert_eq!(cached.change_request, first.change_request);
        assert_eq!(lookup.resolve_count(), 2);
    }

    #[tokio::test]
    async fn authentication_and_rate_limit_keep_the_last_success() {
        let source = source("feature/status");
        let lookup = FakeLookup::new(
            source.clone(),
            [
                Ok(Some(pull_request(90))),
                Err(ChangeRequestError::Authentication),
                Err(ChangeRequestError::RateLimited),
            ],
        );
        let timing = Timing {
            with_change_request_ttl: Duration::from_millis(1),
            without_change_request_ttl: Duration::from_millis(1),
            failure_initial_backoff: Duration::from_millis(1),
            failure_max_backoff: Duration::from_millis(1),
            context_poll_interval: Duration::from_millis(1),
        };
        let service = service(lookup.clone(), timing);
        let key = cache_key("checkout-1", &source);
        let _lease = service.acquire(key.clone());

        let (first, _) = service.refresh(&key, &source).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let (after_auth, _) = service.refresh(&key, &source).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let (after_rate_limit, _) = service.refresh(&key, &source).await;

        assert_eq!(after_auth, first);
        assert_eq!(after_rate_limit, first);
        assert_eq!(lookup.resolve_count(), 3);
    }

    #[test]
    fn failure_backoff_grows_and_caps() {
        let initial = Duration::from_secs(20);
        let maximum = Duration::from_secs(160);
        assert_eq!(
            failure_backoff(initial, maximum, 1),
            Duration::from_secs(20)
        );
        assert_eq!(
            failure_backoff(initial, maximum, 2),
            Duration::from_secs(40)
        );
        assert_eq!(
            failure_backoff(initial, maximum, 3),
            Duration::from_secs(80)
        );
        assert_eq!(
            failure_backoff(initial, maximum, 4),
            Duration::from_secs(160)
        );
        assert_eq!(
            failure_backoff(initial, maximum, 30),
            Duration::from_secs(160)
        );
    }

    #[tokio::test]
    async fn dropping_last_subscriber_evicts_cache_and_stops_refresh_work() {
        let lookup = FakeLookup::new(
            source("feature/status"),
            [Ok(Some(pull_request(90))), Ok(Some(pull_request(91)))],
        );
        let service = service(lookup.clone(), fast_timing());
        let mut stream = service.watch_checkout(PathBuf::from("/checkout"), identity());
        stream.next().await.unwrap();
        drop(stream);

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(lookup.resolve_count(), 1);
        assert!(service.inner.entries.lock().unwrap().is_empty());

        // A later observer gets a fresh entry rather than reviving the
        // abandoned snapshot retained by an inactive checkout.
        let mut renewed = service.watch_checkout(PathBuf::from("/checkout"), identity());
        let renewed_status = renewed.next().await.unwrap();
        assert_eq!(renewed_status.change_request.unwrap().number, 91);
        assert_eq!(lookup.resolve_count(), 2);
    }

    #[tokio::test]
    async fn shutdown_ends_an_active_stream() {
        let lookup = FakeLookup::new(source("feature/status"), [Ok(Some(pull_request(90)))]);
        let service = service(lookup, Timing::default());
        let mut stream = service.watch_checkout(PathBuf::from("/checkout"), identity());
        stream.next().await.unwrap();

        service.shutdown();

        assert!(stream.next().await.is_none());
    }
}
