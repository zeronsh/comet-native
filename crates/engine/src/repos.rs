//! Repos — this device's git repositories, branches, worktrees, and the folder
//! browser (feature-inventory §3.5; port of zeron's `repos.ts` + `folder-lister.ts`).
//!
//! Repos are device-local (paths differ per machine), so the known set is a plain
//! JSON list (`{data_dir}/repos.json`) — no sync. Existing repos can live anywhere
//! the user points us; cloned/created ones land in `{data_dir}/repos`. Worktrees are
//! created under `~/.zeron/worktrees/<repoName>/<worktreeName>` (NOT the data
//! dir — worktrees are user-facing working checkouts), with an auto-generated name +
//! matching `zeron/<name>` branch. `ZERON_WORKTREES_DIR` overrides the root.
//!
//! All git access is via subprocess (`tokio::process`) — never libgit2.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{StreamExt, stream};
use sha2::{Digest, Sha256};

use zeron_proto::{
    DriveEntry, FileSearchMatch, FolderEntry, FolderListing, GitHistoryCommit,
    GitHistoryComparison, GitHistoryPage, GitHistoryRef, GitHistoryRefKind, Repo, RepoRef,
    Worktree,
};

use crate::EngineError;

/// Existence probe timeout for user-chosen / remembered paths, which can point at
/// dead network mounts where a bare `stat` hangs for minutes.
const PATH_EXISTS_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard wall-clock ceiling for a folder listing (the walk runs in a disposable
/// blocking task; on expiry the caller unblocks and the task is abandoned).
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap on returned folder entries (bounds response size).
const FOLDER_LIST_MAX_ENTRIES: usize = 500;
/// Cap on returned drives (a machine with more mounts than this is a server
/// farm, not a laptop picking a project folder).
const DRIVE_LIST_MAX_ENTRIES: usize = 50;
/// File mentions should remain responsive even in very large checkouts.
const FILE_SEARCH_MAX_RESULTS: usize = 8;
/// A dead network mount must not leave the composer search spinning forever.
const FILE_SEARCH_TIMEOUT: Duration = Duration::from_secs(6);
const GITHUB_AVATAR_TIMEOUT: Duration = Duration::from_secs(6);
const FILE_INDEX_TTL: Duration = Duration::from_secs(10);
const FILE_INDEX_MAX_ENTRIES: usize = 250_000;
const RANK_BUFFER: usize = 1_024;
pub const GIT_HISTORY_DEFAULT_LIMIT: usize = 100;
pub const GIT_HISTORY_MAX_LIMIT: usize = 200;

const ADJECTIVES: &[&str] = &[
    "swift", "calm", "bright", "bold", "keen", "brave", "clever", "lucky", "quiet", "warm", "cool",
    "sharp", "gentle", "vivid", "amber", "cobalt",
];
const NOUNS: &[&str] = &[
    "otter", "harbor", "falcon", "cedar", "meadow", "zeron", "delta", "ember", "lynx", "maple",
    "onyx", "quartz", "raven", "summit", "willow", "aspen",
];

/// Canonical identity shared by every chat operating in this exact worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutIdentity {
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)` — device-scoped, path-stable.
    pub id: String,
    /// Canonical worktree root (`rev-parse --show-toplevel`, symlinks resolved).
    pub root: PathBuf,
    /// Canonical git dir (worktree-specific for linked worktrees).
    pub git_dir: PathBuf,
}

/// Best-effort home directory (the `ListFolders` default and worktree root base).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where new worktrees live. Deliberately NOT under the backend data dir —
/// worktrees are user-facing working checkouts. `ZERON_WORKTREES_DIR` overrides
/// (test isolation); empty reads as unset.
fn default_worktrees_root() -> PathBuf {
    std::env::var_os("ZERON_WORKTREES_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".zeron").join("worktrees"))
}

struct ReposInner {
    data_dir: PathBuf,
    device_id: String,
    worktrees_root: PathBuf,
    file_searches: std::sync::Mutex<HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    http: reqwest::Client,
    github_avatars: std::sync::Mutex<HashMap<String, String>>,
    github_avatar_pages: std::sync::Mutex<HashSet<String>>,
    file_index: FileIndexCache,
}

struct IndexedPath {
    path: String,
    haystack: nucleo_matcher::Utf32String,
    is_dir: bool,
}

struct FileIndex {
    entries: Vec<IndexedPath>,
    built: std::time::Instant,
}

type FileIndexCache = std::sync::Mutex<HashMap<PathBuf, std::sync::Arc<FileIndex>>>;

#[derive(Clone)]
pub struct Repos {
    inner: std::sync::Arc<ReposInner>,
}

impl Repos {
    /// `data_dir` holds `repos.json` + cloned/created repos; the worktree root
    /// comes from `$ZERON_WORKTREES_DIR` or `~/.zeron/worktrees`.
    pub fn new(data_dir: &Path, device_id: &str) -> Self {
        Self::with_worktrees_root(data_dir, device_id, default_worktrees_root())
    }

    /// Explicit worktree root (tests).
    pub fn with_worktrees_root(data_dir: &Path, device_id: &str, worktrees_root: PathBuf) -> Self {
        Self {
            inner: std::sync::Arc::new(ReposInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                worktrees_root,
                file_searches: std::sync::Mutex::new(HashMap::new()),
                http: reqwest::Client::builder()
                    .timeout(GITHUB_AVATAR_TIMEOUT)
                    .user_agent("Comet-Git-History")
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
                github_avatars: std::sync::Mutex::new(HashMap::new()),
                github_avatar_pages: std::sync::Mutex::new(HashSet::new()),
                file_index: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    // ── registry (repos.json) ───────────────────────────────────────────────

    fn registry_path(&self) -> PathBuf {
        self.inner.data_dir.join("repos.json")
    }

    fn load_paths(&self) -> Vec<String> {
        std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    fn save_paths(&self, paths: &[String]) -> Result<(), EngineError> {
        let mut seen = HashSet::new();
        let deduped: Vec<&String> = paths.iter().filter(|p| seen.insert(p.as_str())).collect();
        let json = serde_json::to_string_pretty(&deduped)
            .map_err(|e| EngineError::Other(format!("repos registry serialize: {e}")))?;
        std::fs::create_dir_all(&self.inner.data_dir)?;
        std::fs::write(self.registry_path(), json)?;
        Ok(())
    }

    fn register(&self, path: &str) -> Result<(), EngineError> {
        let mut paths = self.load_paths();
        paths.push(path.to_string());
        self.save_paths(&paths)
    }

    // ── git plumbing ────────────────────────────────────────────────────────

    /// Run `git <args>` (optionally under `cwd`), returning trimmed stdout.
    async fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, EngineError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null());
        let output = cmd
            .output()
            .await
            .map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(EngineError::Other(if message.is_empty() {
                format!(
                    "git {} failed ({})",
                    args.first().unwrap_or(&"?"),
                    output.status
                )
            } else {
                format!("git: {message}")
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Async existence probe with a timeout: a wedged network mount just reads
    /// as "gone" instead of hanging every caller.
    async fn path_exists(path: &Path) -> bool {
        let path = path.to_path_buf();
        matches!(
            tokio::time::timeout(PATH_EXISTS_TIMEOUT, tokio::fs::metadata(path)).await,
            Ok(Ok(_))
        )
    }

    /// Is `path` inside a git work tree? (Also the SpacesSync git-presence probe.)
    pub async fn is_repo(&self, path: &Path) -> bool {
        matches!(
            self.git(&["rev-parse", "--is-inside-work-tree"], Some(path)).await,
            Ok(out) if out == "true"
        )
    }

    /// The branch currently checked out at a repo/worktree path (`"HEAD"` when detached).
    pub async fn current_branch(&self, path: &Path) -> Result<String, EngineError> {
        let branch = self.git(&["branch", "--show-current"], Some(path)).await?;
        Ok(if branch.is_empty() {
            "HEAD".to_string()
        } else {
            branch
        })
    }

    /// Commit checked out at `path`, or `None` for a repository without an
    /// initial commit.
    pub async fn head_sha(&self, path: &Path) -> Result<Option<String>, EngineError> {
        match self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(path))
            .await
        {
            Ok(sha) if !sha.is_empty() => Ok(Some(sha)),
            Ok(_) => Ok(None),
            Err(_) if self.is_repo(path).await => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Fetch every configured remote without pruning or integrating anything
    /// into the active branch. This intentionally updates refs only.
    pub async fn fetch_all(&self, repo_path: &Path) -> Result<(), EngineError> {
        self.git(&["fetch", "--all", "--quiet"], Some(repo_path))
            .await
            .map(drop)
    }

    /// The absolute Git `HEAD` file for event-driven external branch reconciliation.
    pub async fn git_head_path(&self, path: &Path) -> Result<PathBuf, EngineError> {
        let git_dir = self
            .git(&["rev-parse", "--absolute-git-dir"], Some(path))
            .await?;
        Ok(PathBuf::from(git_dir).join("HEAD"))
    }

    /// Canonical identity shared by every chat operating in this exact worktree:
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)`.
    pub async fn checkout_identity(&self, path: &Path) -> Result<CheckoutIdentity, EngineError> {
        let root = self
            .git(&["rev-parse", "--show-toplevel"], Some(path))
            .await?;
        let git_dir = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-dir"],
                Some(path),
            )
            .await?;
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));
        let canonical_git_dir =
            std::fs::canonicalize(&git_dir).unwrap_or_else(|_| PathBuf::from(&git_dir));
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical_git_dir.to_string_lossy().as_bytes());
        let id = hex(&hasher.finalize());
        Ok(CheckoutIdentity {
            id,
            root: canonical_root,
            git_dir: canonical_git_dir,
        })
    }

    async fn to_repo(&self, path: &Path) -> Result<Repo, EngineError> {
        let branch = self.current_branch(path).await.ok();
        Ok(Repo {
            path: path.to_string_lossy().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string()),
            default_branch: branch,
        })
    }

    // ── ListRepos / AddRepo / CloneRepo / CreateRepo ────────────────────────

    /// Known repos that still exist, each with its current branch. Never fails:
    /// vanished paths and non-repos are silently dropped.
    pub async fn list(&self) -> Vec<Repo> {
        let mut repos = Vec::new();
        for path in self.load_paths() {
            let path = PathBuf::from(path);
            if !Self::path_exists(&path).await || !self.is_repo(&path).await {
                continue;
            }
            match self.to_repo(&path).await {
                Ok(repo) => repos.push(repo),
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "repo listing skip")
                }
            }
        }
        repos
    }

    /// Remember an existing repository the user pointed us at.
    pub async fn add(&self, path: &str) -> Result<Repo, EngineError> {
        let abs = absolutize(Path::new(path));
        if !Self::path_exists(&abs).await {
            return Err(EngineError::Other(format!(
                "No such folder: {}",
                abs.display()
            )));
        }
        if !self.is_repo(&abs).await {
            return Err(EngineError::Other(format!(
                "Not a git repository: {}",
                abs.display()
            )));
        }
        self.register(&abs.to_string_lossy())?;
        self.to_repo(&abs).await
    }

    /// `git clone <url>` under `{data_dir}/repos`. (Named `clone_repo` to keep
    /// `Clone::clone` unambiguous on the service handle.)
    pub async fn clone_repo(&self, url: &str) -> Result<Repo, EngineError> {
        let trimmed = url.trim().trim_end_matches('/');
        let name = trimmed
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("repo")
            .to_string();
        let repos_dir = self.inner.data_dir.join("repos");
        let target = repos_dir.join(&name);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&repos_dir)?;
        self.git(&["clone", trimmed, &target.to_string_lossy()], None)
            .await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    /// `git init -b main` a fresh repository under `{data_dir}/repos`.
    pub async fn create(&self, name: &str) -> Result<Repo, EngineError> {
        let clean: String = name
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if clean.is_empty() || clean.chars().all(|c| c == '-' || c == '.') {
            return Err(EngineError::Other("Invalid repository name".into()));
        }
        let target = self.inner.data_dir.join("repos").join(&clean);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&target)?;
        self.git(&["init", "-b", "main"], Some(&target)).await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    // ── branches ────────────────────────────────────────────────────────────

    /// All branches (`git branch -a`), local first, deduped against their remote
    /// counterparts, with the repo's default branch first.
    pub async fn branches(&self, repo_path: &Path) -> Result<Vec<String>, EngineError> {
        let out = self
            .git(&["branch", "-a", "--format=%(refname)"], Some(repo_path))
            .await?;
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |name: &str| {
            if !name.is_empty() && name != "HEAD" && seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        };
        // Locals first, then remote-only branches (stripped of their remote prefix).
        for line in out.lines().map(str::trim) {
            if let Some(local) = line.strip_prefix("refs/heads/") {
                push(local);
            }
        }
        for line in out.lines().map(str::trim) {
            if let Some(remote) = line.strip_prefix("refs/remotes/")
                && let Some((_, name)) = remote.split_once('/')
            {
                push(name);
            }
        }
        // Default branch first: origin/HEAD's target, else the checked-out branch.
        let default = match self
            .git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                Some(repo_path),
            )
            .await
        {
            Ok(short) => short.split_once('/').map(|(_, b)| b.to_string()),
            Err(_) => None,
        };
        let default = match default {
            Some(branch) => Some(branch),
            None => self
                .current_branch(repo_path)
                .await
                .ok()
                .filter(|b| b != "HEAD"),
        };
        if let Some(default) = default
            && let Some(pos) = names.iter().position(|n| *n == default)
        {
            let head = names.remove(pos);
            names.insert(0, head);
        }
        Ok(names)
    }

    /// [`Self::branches`] enriched with checkout state: which branch the MAIN
    /// folder has checked out (`current`) and which branches are materialized
    /// as linked worktrees (`worktree_path`). Feeds the composer's ref picker
    /// and its checkout-kind selector.
    pub async fn refs(&self, repo_path: &Path) -> Result<Vec<RepoRef>, EngineError> {
        let names = self.branches(repo_path).await?;
        let current = self.current_branch(repo_path).await.ok();
        // `git worktree list --porcelain`: stanzas of `worktree <path>` /
        // `HEAD <sha>` / `branch refs/heads/<name>`. The first stanza is the
        // main checkout — excluded (it's `current`, not a linked worktree).
        let mut worktrees: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(out) = self
            .git(&["worktree", "list", "--porcelain"], Some(repo_path))
            .await
        {
            let mut stanza = 0usize;
            let mut path: Option<String> = None;
            for line in out.lines().map(str::trim) {
                if let Some(p) = line.strip_prefix("worktree ") {
                    stanza += 1;
                    // The first stanza is the main checkout, not a linked tree.
                    path = (stanza > 1).then(|| p.to_string());
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
                    && let Some(path) = path.take()
                {
                    worktrees.insert(branch.to_string(), path);
                }
            }
        }
        Ok(names
            .into_iter()
            .map(|name| RepoRef {
                current: current.as_deref() == Some(name.as_str()),
                worktree_path: worktrees.get(&name).cloned(),
                name,
            })
            .collect())
    }

    /// Public commit history in topological order. Only user-facing branches,
    /// remotes, and tags seed the walk, so Zeron's internal refs never leak
    /// into the graph or keep otherwise-unreachable checkpoints visible.
    pub async fn history(
        &self,
        repo_path: &Path,
        cursor: usize,
        limit: usize,
    ) -> Result<GitHistoryPage, EngineError> {
        let limit = limit.clamp(1, GIT_HISTORY_MAX_LIMIT);
        let head_sha = self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(repo_path))
            .await
            .ok()
            .filter(|sha| !sha.is_empty());

        let refs_out = self
            .git(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(symref)%00",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                ],
                Some(repo_path),
            )
            .await?;
        let refs_by_sha = parse_history_refs(&refs_out);

        if head_sha.is_none() && refs_by_sha.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                branch_tips: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: Some(0),
                comparison: None,
            });
        }

        let skip = format!("--skip={cursor}");
        let max_count = format!("--max-count={}", limit + 1);
        let mut log_args = vec![
            "log",
            "--topo-order",
            "--no-color",
            "--no-decorate",
            "--no-show-signature",
            "--no-patch",
            skip.as_str(),
            max_count.as_str(),
            "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
        ];
        if head_sha.is_some() {
            log_args.push("HEAD");
        }
        log_args.extend(["--branches", "--remotes", "--tags"]);
        let log = self.git(&log_args, Some(repo_path)).await?;
        let mut commits = parse_history_log(&log, &refs_by_sha);
        let has_next = commits.len() > limit;
        commits.truncate(limit);

        // Branch tips are deliberately independent from history pagination.
        // `--no-walk` resolves every local/remote tip while deduplicating refs
        // that point at the same commit. Include HEAD as a useful anchor for a
        // detached checkout, but do not let tags seed extra overview rows.
        let branch_tips = if cursor == 0 {
            let mut tip_args = vec![
                "log",
                "--no-walk=sorted",
                "--no-color",
                "--no-decorate",
                "--no-show-signature",
                "--no-patch",
                "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
            ];
            if head_sha.is_some() {
                tip_args.push("HEAD");
            }
            tip_args.extend(["--branches", "--remotes"]);
            let tips = self.git(&tip_args, Some(repo_path)).await?;
            parse_history_log(&tips, &refs_by_sha)
        } else {
            Vec::new()
        };

        let total_count = if cursor == 0 {
            let mut count_args = vec!["rev-list", "--count"];
            if head_sha.is_some() {
                count_args.push("HEAD");
            }
            count_args.extend(["--branches", "--remotes", "--tags"]);
            self.git(&count_args, Some(repo_path))
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };
        let head_commit_count = if cursor == 0 && head_sha.is_some() {
            self.git(&["rev-list", "--count", "HEAD"], Some(repo_path))
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };
        let comparison = if cursor == 0 && head_sha.is_some() {
            self.history_comparison(repo_path).await
        } else {
            None
        };

        Ok(GitHistoryPage {
            next_cursor: has_next.then_some(cursor + commits.len()),
            commits,
            branch_tips,
            head_sha,
            total_count,
            head_commit_count,
            comparison,
        })
    }

    /// Compare the checked-out branch with the best locally available
    /// integration ref. This deliberately never talks to the network: `Fetch
    /// all` refreshes remote-tracking refs, then the next History load sees
    /// those new counts.
    async fn history_comparison(&self, repo_path: &Path) -> Option<GitHistoryComparison> {
        // Detached checkouts do not have a branch relationship to present.
        self.git(
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            Some(repo_path),
        )
        .await
        .ok()?;

        // An integration remote is more useful than the branch's push/tracking
        // ref: a feature usually tracks origin/feature, whereas the status the
        // user needs in History is its relationship to upstream/main.
        let mut candidates = Vec::new();
        for remote in ["upstream", "origin"] {
            let remote_head = format!("refs/remotes/{remote}/HEAD");
            if let Ok(base) = self
                .git(
                    &["symbolic-ref", "--quiet", "--short", &remote_head],
                    Some(repo_path),
                )
                .await
                && !base.is_empty()
            {
                candidates.push(base);
            }
        }
        // A remote HEAD is not guaranteed to have been configured locally.
        // Conventional default names keep the result useful in that case.
        candidates.extend([
            "upstream/main".to_string(),
            "origin/main".to_string(),
            "upstream/master".to_string(),
            "origin/master".to_string(),
        ]);
        // Fall back to the configured tracking ref only after integration
        // defaults. This still gives sensible data in a single-remote repo.
        if let Ok(base) = self
            .git(
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                Some(repo_path),
            )
            .await
            && !base.is_empty()
        {
            candidates.push(base);
        }

        let mut seen = HashSet::new();
        for base in candidates
            .into_iter()
            .filter(|base| seen.insert(base.clone()))
        {
            let commit_ref = format!("{base}^{{commit}}");
            if self
                .git(
                    &["rev-parse", "--verify", "--quiet", &commit_ref],
                    Some(repo_path),
                )
                .await
                .is_err()
            {
                continue;
            }
            let range = format!("HEAD...{base}");
            let Ok(counts) = self
                .git(
                    &["rev-list", "--left-right", "--count", &range],
                    Some(repo_path),
                )
                .await
            else {
                continue;
            };
            let mut counts = counts.split_whitespace();
            let (Some(ahead), Some(behind)) = (counts.next(), counts.next()) else {
                continue;
            };
            let (Ok(ahead), Ok(behind)) = (ahead.parse(), behind.parse()) else {
                continue;
            };
            return Some(GitHistoryComparison {
                base,
                ahead,
                behind,
            });
        }
        None
    }

    /// Fuzzy subject / SHA search across the complete public history. Results
    /// stay in `--topo-order`; fuzzy score decides inclusion, never row order,
    /// so the client can keep rendering a meaningful commit graph.
    pub async fn search_history(
        &self,
        repo_path: &Path,
        query: &str,
        cursor: usize,
        limit: usize,
    ) -> Result<GitHistoryPage, EngineError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                branch_tips: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: None,
                comparison: None,
            });
        }
        let limit = limit.clamp(1, GIT_HISTORY_MAX_LIMIT);
        let head_sha = self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(repo_path))
            .await
            .ok()
            .filter(|sha| !sha.is_empty());
        let refs_out = self
            .git(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(symref)%00",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                ],
                Some(repo_path),
            )
            .await?;
        let refs_by_sha = parse_history_refs(&refs_out);
        if head_sha.is_none() && refs_by_sha.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                branch_tips: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: None,
                comparison: None,
            });
        }

        let mut log_args = vec![
            "log",
            "--topo-order",
            "--no-color",
            "--no-decorate",
            "--no-show-signature",
            "--no-patch",
            "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
        ];
        if head_sha.is_some() {
            log_args.push("HEAD");
        }
        log_args.extend(["--branches", "--remotes", "--tags"]);
        let log = self.git(&log_args, Some(repo_path)).await?;
        let all_commits = parse_history_log(&log, &refs_by_sha);
        let visible: HashSet<String> = all_commits
            .iter()
            .filter(|commit| git_history_matches(query, commit))
            .map(|commit| commit.sha.clone())
            .collect();
        let matches = compact_history_commits(&all_commits, &visible);
        let total_count = matches.len();
        let start = cursor.min(total_count);
        let end = start.saturating_add(limit).min(total_count);
        let commits = matches[start..end].to_vec();

        Ok(GitHistoryPage {
            commits,
            branch_tips: Vec::new(),
            head_sha,
            next_cursor: (end < total_count).then_some(end),
            total_count: Some(total_count),
            head_commit_count: None,
            comparison: None,
        })
    }

    /// Best-effort GitHub profile images for the authors in one history page.
    /// Git itself only stores names and emails, so this resolves the hosting
    /// metadata separately and caches it by both commit and normalized email.
    pub async fn history_avatar_urls(
        &self,
        repo_path: &Path,
        authors: &[(String, String)],
        cursor: usize,
        limit: usize,
    ) -> HashMap<String, String> {
        let Ok(remote) = self
            .git(&["remote", "get-url", "origin"], Some(repo_path))
            .await
        else {
            return HashMap::new();
        };
        let Some((owner, repo)) = parse_github_remote(&remote) else {
            return HashMap::new();
        };
        let repo_key = format!("{owner}/{repo}").to_ascii_lowercase();
        let per_page = limit.clamp(1, 100);
        let page = cursor / per_page + 1;
        let page_key = format!("{repo_key}|{page}|{per_page}");
        let should_fetch = self
            .inner
            .github_avatar_pages
            .lock()
            .map(|mut pages| pages.insert(page_key.clone()))
            .unwrap_or(false);

        if should_fetch {
            let url = format!("https://api.github.com/repos/{owner}/{repo}/commits");
            let mut request = self.inner.http.get(url).query(&[
                ("per_page", per_page.to_string()),
                ("page", page.to_string()),
            ]);
            if let Some(token) = std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|token| !token.is_empty())
                .or_else(|| {
                    std::env::var("GH_TOKEN")
                        .ok()
                        .filter(|token| !token.is_empty())
                })
            {
                request = request.bearer_auth(token);
            }

            let rows = match request.send().await {
                Ok(response) if response.status().is_success() => {
                    response.json::<Vec<GitHubCommitAvatar>>().await.ok()
                }
                _ => None,
            };
            if let Some(rows) = rows {
                let mut identities_by_url: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for row in rows {
                    let Some(author) = row.author else {
                        continue;
                    };
                    if !author.avatar_url.starts_with("https://") {
                        continue;
                    }
                    let avatar_url = if author.avatar_url.contains('?') {
                        format!("{}&s=40", author.avatar_url)
                    } else {
                        format!("{}?s=40", author.avatar_url)
                    };
                    identities_by_url.entry(avatar_url).or_default().push((
                        row.sha.to_ascii_lowercase(),
                        row.commit.author.email.trim().to_ascii_lowercase(),
                    ));
                }
                let downloads = stream::iter(identities_by_url.into_iter().map(
                    |(url, identities)| async move {
                        self.cache_github_avatar(&url)
                            .await
                            .map(|path| (identities, path))
                    },
                ))
                .buffer_unordered(8)
                .filter_map(|download| async move { download })
                .collect::<Vec<_>>()
                .await;
                if let Ok(mut cache) = self.inner.github_avatars.lock() {
                    for (identities, path) in downloads {
                        for (sha, email) in identities {
                            cache.insert(format!("{repo_key}|sha|{sha}"), path.clone());
                            if !email.is_empty() {
                                cache.insert(format!("{repo_key}|email|{email}"), path.clone());
                            }
                        }
                    }
                }
            } else if let Ok(mut pages) = self.inner.github_avatar_pages.lock() {
                // A transient network/auth failure may be retried on refresh.
                pages.remove(&page_key);
            }
        }

        let Ok(cache) = self.inner.github_avatars.lock() else {
            return HashMap::new();
        };
        authors
            .iter()
            .filter_map(|(sha, email)| {
                let avatar = cache
                    .get(&format!("{repo_key}|sha|{}", sha.to_ascii_lowercase()))
                    .or_else(|| {
                        cache.get(&format!(
                            "{repo_key}|email|{}",
                            email.trim().to_ascii_lowercase()
                        ))
                    })?;
                Some((email.trim().to_ascii_lowercase(), avatar.clone()))
            })
            .collect()
    }

    async fn cache_github_avatar(&self, url: &str) -> Option<String> {
        const MAX_AVATAR_BYTES: u64 = 2 * 1024 * 1024;

        let digest = Sha256::digest(url.as_bytes());
        let cache_dir = self.inner.data_dir.join("cache").join("git-avatars");
        let path = cache_dir.join(format!("{}.img", hex(&digest[..16])));
        if tokio::fs::metadata(&path)
            .await
            .ok()
            .is_some_and(|metadata| metadata.len() > 0)
        {
            return Some(path.to_string_lossy().into_owned());
        }
        tokio::fs::create_dir_all(&cache_dir).await.ok()?;
        let response = self.inner.http.get(url).send().await.ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > MAX_AVATAR_BYTES)
        {
            return None;
        }
        let bytes = response.bytes().await.ok()?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_AVATAR_BYTES {
            return None;
        }
        let temporary = cache_dir.join(format!(
            ".{}.{}.tmp",
            hex(&digest[..8]),
            uuid::Uuid::new_v4()
        ));
        tokio::fs::write(&temporary, &bytes).await.ok()?;
        if tokio::fs::rename(&temporary, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
            if !path.is_file() {
                return None;
            }
        }
        Some(path.to_string_lossy().into_owned())
    }

    /// Whether `candidate` is the repository root or one of its linked
    /// worktrees. Filesystem resolution happens on a disposable thread because
    /// user-selected paths may be dead mounts.
    pub async fn workspace_checkout(&self, repo_path: &Path, candidate: &Path) -> Option<PathBuf> {
        let repo_path = repo_path.to_path_buf();
        let candidate = candidate.to_path_buf();
        let worktrees: Vec<_> = self
            .refs(&repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.worktree_path.map(PathBuf::from))
            .collect();
        disposable_worker("checkout-auth", move || {
            let candidate = std::fs::canonicalize(candidate).ok()?;
            std::iter::once(repo_path)
                .chain(worktrees)
                .filter_map(|path| std::fs::canonicalize(path).ok())
                .any(|path| path == candidate)
                .then_some(candidate)
        })
        .await
        .flatten()
    }

    /// Switch the checkout at `cwd` (a main folder OR a linked worktree) to
    /// `ref_name` — the t3code `switchRef` port: an existing local branch is
    /// checked out directly; a remote-only branch gets a local tracking
    /// branch (`checkout --track origin/<ref>`). A dirty tree or a branch
    /// already checked out in another worktree fails with git's own message.
    /// Returns the resulting current branch.
    pub async fn switch_ref(&self, cwd: &Path, ref_name: &str) -> Result<String, EngineError> {
        let local = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{ref_name}"),
                ],
                Some(cwd),
            )
            .await
            .is_ok();
        if local {
            self.git(&["checkout", ref_name], Some(cwd)).await?;
        } else {
            let remote = format!("origin/{ref_name}");
            let has_remote = self
                .git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/{remote}"),
                    ],
                    Some(cwd),
                )
                .await
                .is_ok();
            if has_remote {
                self.git(&["checkout", "--track", &remote], Some(cwd))
                    .await?;
            } else {
                // Unknown ref: let git produce the authoritative error.
                self.git(&["checkout", ref_name], Some(cwd)).await?;
            }
        }
        let out = self.git(&["branch", "--show-current"], Some(cwd)).await?;
        Ok(out.trim().to_string())
    }

    // ── worktrees ───────────────────────────────────────────────────────────

    /// `git worktree add` an isolated checkout under
    /// `{worktrees_root}/<repoName>/<generatedName>`, on a fresh `zeron/<name>`
    /// branch off `branch`.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<Worktree, EngineError> {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let base = self.inner.worktrees_root.join(&repo_name);
        std::fs::create_dir_all(&base)?;
        // Auto-generate a name colliding with neither an existing dir nor branch.
        let existing: HashSet<String> = self
            .branches(repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut name = None;
        for attempt in 0..50u64 {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(attempt)
                .wrapping_add(attempt.wrapping_mul(0x9E37_79B9));
            let candidate = format!(
                "{}-{}",
                ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize],
                NOUNS[((seed / 31) % NOUNS.len() as u64) as usize]
            );
            if !base.join(&candidate).exists() && !existing.contains(&format!("zeron/{candidate}"))
            {
                name = Some(candidate);
                break;
            }
        }
        let name =
            name.ok_or_else(|| EngineError::Other("Could not allocate a worktree name".into()))?;
        let path = base.join(&name);
        let branch_name = format!("zeron/{name}");
        self.git(
            &[
                "worktree",
                "add",
                "-b",
                &branch_name,
                &path.to_string_lossy(),
                branch,
            ],
            Some(repo_path),
        )
        .await?;
        let checkout = self.checkout_identity(&path).await?;
        Ok(Worktree {
            repo_path: repo_path.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
            branch: branch_name,
            name,
            checkout_id: Some(checkout.id),
        })
    }

    async fn branch_exists(&self, path: &Path, branch: &str) -> bool {
        self.git(
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            Some(path),
        )
        .await
        .is_ok()
    }

    /// Rename a zeron-created worktree branch after its chat's generated title
    /// (port of zeron's `renameWorktreeBranch`). Guards:
    /// - respect an external checkout/rename: only act while the worktree is still
    ///   on `expected_branch` AND that branch is the original `zeron/<folderName>`;
    /// - a title-slug collision gets a stable 6-hex suffix (hash of the worktree
    ///   path); a collision on THAT too fails.
    ///
    /// Returns the branch the worktree ends up on (re-read after the rename so a
    /// concurrent external checkout always wins the metadata race).
    pub async fn rename_worktree_branch(
        &self,
        worktree_path: &Path,
        expected_branch: &str,
        title: &str,
    ) -> Result<String, EngineError> {
        let current = self.current_branch(worktree_path).await?;
        let folder = worktree_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if current != expected_branch || expected_branch != format!("zeron/{folder}") {
            return Ok(current);
        }
        let preferred = worktree_branch_from_title(title);
        if preferred == current {
            return Ok(current);
        }
        let mut hasher = Sha256::new();
        hasher.update(worktree_path.to_string_lossy().as_bytes());
        let suffix = &hex(&hasher.finalize())[..6];
        let target = if self.branch_exists(worktree_path, &preferred).await {
            format!("{preferred}-{suffix}")
        } else {
            preferred
        };
        if self.branch_exists(worktree_path, &target).await {
            return Err(EngineError::Other(format!(
                "Branch already exists: {target}"
            )));
        }
        self.git(
            &["branch", "-m", "--", &current, &target],
            Some(worktree_path),
        )
        .await?;
        self.current_branch(worktree_path).await
    }

    /// Best-effort worktree removal (if it still exists), then prune stale refs.
    /// Deletes the worktree's branch ONLY when zeron created it (`zeron/…`) — the
    /// user may have checked out their own branch inside the worktree.
    pub async fn delete_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        let branch = if worktree_path.exists() {
            self.current_branch(worktree_path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if worktree_path.exists() {
            let removed = self
                .git(
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ],
                    Some(repo_path),
                )
                .await;
            if removed.is_err() {
                // git refused (or the dir is half-gone) — delete the folder directly.
                let _ = std::fs::remove_dir_all(worktree_path);
            }
        }
        let _ = self.git(&["worktree", "prune"], Some(repo_path)).await;
        if branch.starts_with("zeron/") {
            let _ = self.git(&["branch", "-D", &branch], Some(repo_path)).await;
        }
        Ok(())
    }

    // ── ListFolders ─────────────────────────────────────────────────────────

    /// One directory level (home by default): dotfiles hidden, directories first,
    /// capped at [`FOLDER_LIST_MAX_ENTRIES`] with a `truncated` flag. The walk runs
    /// in a spawned blocking task under a 6s wall-clock ceiling — a wedged path
    /// (dead mount, permission-gated folder) fails this listing without blocking
    /// anything else; the abandoned task unwinds on its own thread.
    pub async fn list_folders(&self, path: Option<String>) -> Result<FolderListing, EngineError> {
        self.list_folders_with(path, FOLDER_LIST_TIMEOUT, false)
            .await
    }

    /// Search a checkout's files and directories by fuzzy relative path. The
    /// `ignore` walker honors `.gitignore`, `.ignore`, and global git excludes.
    /// Dotfiles remain searchable; only repository metadata is always pruned.
    pub async fn search_files(
        &self,
        root: PathBuf,
        query: String,
        featured_paths: Vec<String>,
    ) -> Result<Vec<FileSearchMatch>, EngineError> {
        let deadline = tokio::time::Instant::now() + FILE_SEARCH_TIMEOUT;
        let gate = {
            let mut searches = self
                .inner
                .file_searches
                .lock()
                .map_err(|_| EngineError::Other("file search registry poisoned".into()))?;
            if let Some(gate) = searches.get(&root).and_then(std::sync::Weak::upgrade) {
                gate
            } else {
                let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                searches.insert(root.clone(), std::sync::Arc::downgrade(&gate));
                gate
            }
        };
        let gate = tokio::time::timeout_at(deadline, gate.lock_owned())
            .await
            .map_err(|_| EngineError::Other("file search timed out".into()))?;
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let worker_cancelled = cancelled.clone();
        let cache = self.inner.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("file-search".into())
            .spawn(move || {
                let _gate = gate;
                let _ = tx.send(search_files_cached(
                    &cache.file_index,
                    &root,
                    &query,
                    &featured_paths,
                    || worker_cancelled.load(Ordering::Relaxed),
                ));
            })
            .map_err(|e| EngineError::Other(format!("file search failed: {e}")))?;
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::Other("file search worker exited".into())),
            Err(_) => Err(EngineError::Other("file search timed out".into())),
        }
    }

    /// `hang_for_test` makes the worker never respond — exercises the timeout path.
    ///
    /// The walk runs on a DETACHED OS thread (not the tokio blocking pool): a
    /// readdir wedged in the kernel can't be cancelled, and a poisoned blocking
    /// pool — or a runtime shutdown waiting on it — must never be possible. On
    /// timeout the thread is simply abandoned (the zeron backend's disposable
    /// worker, minus the terminate()).
    #[doc(hidden)]
    pub async fn list_folders_with(
        &self,
        path: Option<String>,
        timeout: Duration,
        hang_for_test: bool,
    ) -> Result<FolderListing, EngineError> {
        let target = match path.filter(|p| !p.trim().is_empty()) {
            Some(p) => absolutize(Path::new(&p)),
            None => home_dir(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("folder-list".into())
            .spawn(move || {
                if hang_for_test {
                    // Hold the sender without responding (detached thread; process
                    // exit reclaims it) — the caller must hit its timeout.
                    std::thread::sleep(Duration::from_secs(3600));
                }
                let _ = tx.send(list_folders_blocking(&target));
            });
        if let Err(err) = spawned {
            return Err(EngineError::Other(format!("folder listing failed: {err}")));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::Other("folder listing worker exited".into())),
            Err(_) => Err(EngineError::Other(
                "folder listing timed out on the device".into(),
            )),
        }
    }

    // ── ListDrives ──────────────────────────────────────────────────────────

    /// Mounted drives/volumes — the browse roots beyond home. Same disposable
    /// worker + wall-clock ceiling as `ListFolders`: a dead network mount can
    /// wedge the probe (macOS `canonicalize` stats each volume), and that must
    /// fail this listing, not the runtime.
    pub async fn list_drives(&self) -> Result<Vec<DriveEntry>, EngineError> {
        let worker = disposable_worker("drive-list", list_drives_blocking);
        match tokio::time::timeout(FOLDER_LIST_TIMEOUT, worker).await {
            Ok(Some(drives)) => Ok(drives),
            Ok(None) => Err(EngineError::Other("drive listing worker exited".into())),
            Err(_) => Err(EngineError::Other(
                "drive listing timed out on the device".into(),
            )),
        }
    }
}

struct CancelOnDrop(std::sync::Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn disposable_worker<T: Send + 'static>(
    name: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.await.ok()
}

/// The blocking walk: ONE readdir of the target; `is_repo` is a cheap `.git`
/// existence probe per directory entry.
fn list_folders_blocking(target: &Path) -> Result<FolderListing, EngineError> {
    let read = std::fs::read_dir(target).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            EngineError::Other("Zeron doesn't have access to this folder on the device.".into())
        }
        _ => EngineError::Other(format!("could not read that folder: {e}")),
    })?;
    let mut entries: Vec<FolderEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_repo = is_dir && entry.path().join(".git").exists();
        entries.push(FolderEntry {
            name,
            is_dir,
            is_repo,
        });
    }
    // Directories first, each group name-sorted (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FOLDER_LIST_MAX_ENTRIES;
    entries.truncate(FOLDER_LIST_MAX_ENTRIES);
    Ok(FolderListing {
        path: target.to_string_lossy().to_string(),
        entries,
        truncated,
    })
}

/// The blocking drive walk. Best-effort everywhere: the list is navigation
/// sugar for the folder browser, so an unreadable source means fewer rows,
/// never an error (home always remains reachable without it).
fn list_drives_blocking() -> Vec<DriveEntry> {
    #[cfg(target_os = "macos")]
    let drives = macos_drives();
    #[cfg(target_os = "linux")]
    let drives = linux_drives(&std::fs::read_to_string("/proc/mounts").unwrap_or_default());
    #[cfg(windows)]
    let drives = windows_drives();
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let drives = Vec::new();
    finish_drives(drives)
}

/// `/Volumes` holds every mounted volume; the boot volume is a symlink to `/`,
/// which `canonicalize` resolves — so the system drive arrives under its real
/// name ("Macintosh HD") and the fallback "System" row never shows on macOS.
#[cfg(target_os = "macos")]
fn macos_drives() -> Vec<DriveEntry> {
    let mut drives: Vec<DriveEntry> = Vec::new();
    if let Ok(read) = std::fs::read_dir("/Volumes") {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Follows the symlink — a dangling one (mid-eject) drops out here.
            if !path.is_dir() {
                continue;
            }
            let resolved = std::fs::canonicalize(&path).unwrap_or(path);
            drives.push(DriveEntry {
                name,
                path: resolved.to_string_lossy().to_string(),
            });
        }
    }
    drives
}

/// Top-level directories the FHS (or its de-facto extensions) owns. A
/// depth-one mount point OUTSIDE this set is a user-created drive root
/// (`/disk2`, `/data`, `/tank`) — the system's own split partitions
/// (`/boot`, a separate `/var` or `/home`) are plumbing, not drives.
#[cfg(any(target_os = "linux", test))]
const FHS_TOP_LEVEL: &[&str] = &[
    "bin",
    "boot",
    "dev",
    "efi",
    "etc",
    "home",
    "lib",
    "lib32",
    "lib64",
    "libx32",
    "lost+found",
    "media",
    "mnt",
    "nix",
    "opt",
    "proc",
    "root",
    "run",
    "sbin",
    "snap",
    "srv",
    "sys",
    "tmp",
    "usr",
    "var",
];

/// Mounted drives from `/proc/mounts`: the conventional removable locations
/// (`/media`, `/run/media`, `/mnt` — udisks, WSL drive letters, manual
/// mounts, whatever the filesystem) plus block-device partitions mounted at
/// custom top-level paths like `/disk2` (PR #144 feedback) — `/dev/*`
/// sources at depth-one non-FHS points, minus squashfs/erofs images (snaps).
/// Plus "System" for `/`: anything mounted deeper stays reachable by
/// browsing from the root, and the palette accepts typed paths besides.
#[cfg(any(target_os = "linux", test))]
fn linux_drives(mounts: &str) -> Vec<DriveEntry> {
    let mut drives: Vec<DriveEntry> = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(point), fstype) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let point = unescape_mount_point(point);
        let conventional = point.starts_with("/media/")
            || point.starts_with("/run/media/")
            || point == "/mnt"
            || point.starts_with("/mnt/");
        let custom_block = source.starts_with("/dev/")
            && !matches!(fstype, Some("squashfs") | Some("erofs"))
            && point
                .strip_prefix('/')
                .is_some_and(|p| !p.is_empty() && !p.contains('/') && !FHS_TOP_LEVEL.contains(&p));
        if !conventional && !custom_block {
            continue;
        }
        let name = point
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("Drive")
            .to_string();
        drives.push(DriveEntry { name, path: point });
    }
    drives.push(DriveEntry {
        name: "System".into(),
        path: "/".into(),
    });
    drives
}

/// Probe the drive letters. `exists` on a wedged network letter can hang —
/// covered by the caller's disposable-worker timeout.
#[cfg(windows)]
fn windows_drives() -> Vec<DriveEntry> {
    ('A'..='Z')
        .filter_map(|letter| {
            let path = format!("{letter}:\\");
            std::path::Path::new(&path).exists().then(|| DriveEntry {
                name: format!("{letter}:"),
                path,
            })
        })
        .collect()
}

/// Dedupe by mount point (first mention wins — the named row beats a
/// late generic duplicate), system root first, then name order, capped.
fn finish_drives(mut drives: Vec<DriveEntry>) -> Vec<DriveEntry> {
    let mut seen = HashSet::new();
    drives.retain(|d| seen.insert(d.path.clone()));
    drives.sort_by(|a, b| {
        (a.path != "/")
            .cmp(&(b.path != "/"))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    drives.truncate(DRIVE_LIST_MAX_ENTRIES);
    drives
}

/// `/proc/mounts` octal-escapes whitespace in mount points (`\040` = space).
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_point(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = chars.clone().take(3).collect();
            if digits.len() == 3
                && let Ok(code) = u8::from_str_radix(&digits, 8)
            {
                out.push(code as char);
                chars.nth(2);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Case-insensitive subsequence score. Lower is better: adjacent and earlier
/// characters win, while still allowing `cmp rs` to find `composer.rs`.
fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let candidate = candidate.to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .try_fold(0usize, |total, term| {
            let mut at = 0;
            let mut score = 0usize;
            let mut previous_end = None;
            for needle in term.chars() {
                let found = candidate[at..].find(needle)? + at;
                score += found;
                if previous_end == Some(found) {
                    score = score.saturating_sub(2);
                }
                at = found + needle.len_utf8();
                previous_end = Some(at);
            }
            Some(total.saturating_add(score))
        })
}

/// Match a history commit using the same semantics as the history UI.
///
/// This remains the shared entry point so RPC filtering and client-side
/// filtering cannot disagree about Unicode case normalization.
pub fn git_history_matches(query: &str, commit: &GitHistoryCommit) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let normalized = query.to_ascii_lowercase();
    commit.sha.to_ascii_lowercase().starts_with(&normalized)
        || fuzzy_score(query, &format!("{} {}", commit.sha, commit.subject)).is_some()
}

/// Contract hidden commits to their nearest visible ancestors. Search results
/// remain sparse without leaving graph lanes aimed at rows that are absent.
fn compact_history_commits(
    commits: &[GitHistoryCommit],
    visible: &HashSet<String>,
) -> Vec<GitHistoryCommit> {
    let by_sha: HashMap<_, _> = commits
        .iter()
        .map(|commit| (commit.sha.as_str(), commit))
        .collect();

    /// Resolve a hidden commit to its nearest visible ancestors without using
    /// the call stack. Completed nodes are cached across visible commits so a
    /// shared hidden branch is contracted only once.
    fn nearest_visible_parents(
        sha: &str,
        visible: &HashSet<String>,
        by_sha: &HashMap<&str, &GitHistoryCommit>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if visible.contains(sha) || !by_sha.contains_key(sha) {
            return vec![sha.to_string()];
        }
        if let Some(cached) = memo.get(sha) {
            return cached.clone();
        }

        struct Frame {
            sha: String,
            next_parent: usize,
            resolved: Vec<String>,
            seen: HashSet<String>,
        }

        impl Frame {
            fn new(sha: String) -> Self {
                Self {
                    sha,
                    next_parent: 0,
                    resolved: Vec::new(),
                    seen: HashSet::new(),
                }
            }

            fn extend(&mut self, parents: &[String]) {
                for parent in parents {
                    if self.seen.insert(parent.clone()) {
                        self.resolved.push(parent.clone());
                    }
                }
            }
        }

        let mut visiting = HashSet::from([sha.to_string()]);
        let mut stack = vec![Frame::new(sha.to_string())];
        loop {
            let next_parent = {
                let frame = stack.last_mut().expect("history traversal frame");
                let parents = &by_sha[frame.sha.as_str()].parent_shas;
                (frame.next_parent < parents.len()).then(|| {
                    let parent = parents[frame.next_parent].clone();
                    frame.next_parent += 1;
                    parent
                })
            };

            let Some(parent) = next_parent else {
                let frame = stack.pop().expect("history traversal frame");
                visiting.remove(&frame.sha);
                let resolved = frame.resolved;
                memo.insert(frame.sha, resolved.clone());
                if let Some(caller) = stack.last_mut() {
                    caller.extend(&resolved);
                    continue;
                }
                return resolved;
            };

            let resolved = if visible.contains(&parent) || !by_sha.contains_key(parent.as_str()) {
                Some(vec![parent.clone()])
            } else if let Some(cached) = memo.get(&parent) {
                Some(cached.clone())
            } else if visiting.contains(&parent) {
                // Git commit graphs are acyclic, but keep malformed input from
                // looping forever just as the previous `visiting` guard did.
                Some(Vec::new())
            } else {
                None
            };

            if let Some(resolved) = resolved {
                stack
                    .last_mut()
                    .expect("history traversal frame")
                    .extend(&resolved);
            } else {
                visiting.insert(parent.clone());
                stack.push(Frame::new(parent));
            }
        }
    }

    let mut memo = HashMap::new();
    commits
        .iter()
        .filter(|commit| visible.contains(&commit.sha))
        .cloned()
        .map(|mut commit| {
            let mut seen = HashSet::new();
            commit.parent_shas = commit
                .parent_shas
                .iter()
                .flat_map(|parent| nearest_visible_parents(parent, visible, &by_sha, &mut memo))
                .filter(|parent| seen.insert(parent.clone()))
                .collect();
            commit
        })
        .collect()
}

type RankedFileMatch = (Option<usize>, u32, String, bool);

fn compare_file_matches(
    query: &str,
    (featured_a, score_a, path_a, dir_a): &RankedFileMatch,
    (featured_b, score_b, path_b, dir_b): &RankedFileMatch,
) -> std::cmp::Ordering {
    let empty_query = query.trim().is_empty();
    featured_a
        .is_none()
        .cmp(&featured_b.is_none())
        .then_with(|| featured_a.cmp(featured_b))
        .then_with(|| score_b.cmp(score_a))
        .then_with(|| {
            empty_query
                .then(|| path_a.split('/').count().cmp(&path_b.split('/').count()))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            empty_query
                .then(|| dir_a.cmp(dir_b))
                .unwrap_or_else(|| dir_b.cmp(dir_a))
        })
        .then_with(|| path_a.len().cmp(&path_b.len()))
        .then_with(|| path_a.cmp(path_b))
}

#[cfg(test)]
fn search_files_blocking(
    root: &Path,
    query: &str,
    featured_paths: &[String],
) -> Result<Vec<FileSearchMatch>, EngineError> {
    search_files_blocking_with_cancel(root, query, featured_paths, || false)
}

#[cfg(test)]
fn search_files_blocking_with_cancel<F: Fn() -> bool + Sync>(
    root: &Path,
    query: &str,
    featured_paths: &[String],
    cancelled: F,
) -> Result<Vec<FileSearchMatch>, EngineError> {
    let root = canonical_search_root(root)?;
    let index = walk_file_index(&root, &cancelled)?;
    Ok(rank_file_matches(&index, &root, query, featured_paths))
}

fn search_files_cached<F: Fn() -> bool + Sync>(
    cache: &FileIndexCache,
    root: &Path,
    query: &str,
    featured_paths: &[String],
    cancelled: F,
) -> Result<Vec<FileSearchMatch>, EngineError> {
    let root = canonical_search_root(root)?;
    let fresh = cache
        .lock()
        .ok()
        .and_then(|indexes| indexes.get(&root).cloned())
        .filter(|index| index.built.elapsed() < FILE_INDEX_TTL);
    let index = match fresh {
        Some(index) => index,
        None => {
            let index = std::sync::Arc::new(FileIndex {
                entries: walk_file_index(&root, &cancelled)?,
                built: std::time::Instant::now(),
            });
            if let Ok(mut indexes) = cache.lock() {
                indexes.retain(|_, index| index.built.elapsed() < FILE_INDEX_TTL);
                indexes.insert(root.clone(), index.clone());
            }
            index
        }
    };
    if cancelled() {
        return Err(EngineError::Other("file search cancelled".into()));
    }
    Ok(rank_file_matches(
        &index.entries,
        &root,
        query,
        featured_paths,
    ))
}

fn canonical_search_root(root: &Path) -> Result<PathBuf, EngineError> {
    std::fs::canonicalize(root)
        .map_err(|e| EngineError::Other(format!("could not search workspace: {e}")))
}

fn walk_file_index<F: Fn() -> bool + Sync>(
    root: &Path,
    cancelled: &F,
) -> Result<Vec<IndexedPath>, EngineError> {
    if cancelled() {
        return Err(EngineError::Other("file search cancelled".into()));
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(12))
        .unwrap_or(4);
    let collected: std::sync::Mutex<Vec<IndexedPath>> = std::sync::Mutex::new(Vec::new());
    let was_cancelled = AtomicBool::new(false);
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .threads(threads)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
        .build_parallel()
        .run(|| {
            const BATCH: usize = 512;
            struct Batch<'a> {
                items: Vec<IndexedPath>,
                sink: &'a std::sync::Mutex<Vec<IndexedPath>>,
            }
            impl Drop for Batch<'_> {
                fn drop(&mut self) {
                    if self.items.is_empty() {
                        return;
                    }
                    if let Ok(mut all) = self.sink.lock() {
                        all.append(&mut self.items);
                    }
                }
            }
            let mut batch = Batch {
                items: Vec::with_capacity(BATCH),
                sink: &collected,
            };
            let was_cancelled = &was_cancelled;
            Box::new(move |entry| {
                if was_cancelled.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }
                if cancelled() {
                    was_cancelled.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        tracing::debug!(%err, "file mention index skipped entry");
                        return ignore::WalkState::Continue;
                    }
                };
                let path = entry.path();
                if path == root {
                    return ignore::WalkState::Continue;
                }
                let Ok(relative) = path.strip_prefix(root) else {
                    return ignore::WalkState::Continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                if relative == ".git" || relative.starts_with(".git/") {
                    return ignore::WalkState::Continue;
                }
                batch.items.push(IndexedPath {
                    haystack: nucleo_matcher::Utf32String::from(relative.as_str()),
                    path: relative,
                    is_dir: entry.file_type().is_some_and(|kind| kind.is_dir()),
                });
                if batch.items.len() >= BATCH
                    && let Ok(mut all) = batch.sink.lock()
                {
                    all.append(&mut batch.items);
                    if all.len() >= FILE_INDEX_MAX_ENTRIES {
                        return ignore::WalkState::Quit;
                    }
                }
                ignore::WalkState::Continue
            })
        });
    if was_cancelled.load(Ordering::Relaxed) || cancelled() {
        return Err(EngineError::Other("file search cancelled".into()));
    }
    let mut entries = collected
        .into_inner()
        .map_err(|_| EngineError::Other("file index poisoned".into()))?;
    entries.truncate(FILE_INDEX_MAX_ENTRIES);
    Ok(entries)
}

fn rank_file_matches(
    entries: &[IndexedPath],
    root: &Path,
    query: &str,
    featured_paths: &[String],
) -> Vec<FileSearchMatch> {
    let featured: HashMap<String, usize> = featured_paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let canonical = std::fs::canonicalize(full).ok()?;
            let relative = canonical.strip_prefix(root).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .enumerate()
        .fold(HashMap::new(), |mut paths, (rank, path)| {
            paths.entry(path).or_insert(rank);
            paths
        });
    let mut matcher = nucleo_matcher::Matcher::new({
        let mut config = nucleo_matcher::Config::DEFAULT;
        config.set_match_paths();
        config
    });
    let pattern = nucleo_matcher::pattern::Pattern::parse(
        query,
        nucleo_matcher::pattern::CaseMatching::Smart,
        nucleo_matcher::pattern::Normalization::Smart,
    );
    let mut matches: Vec<RankedFileMatch> = Vec::new();
    for entry in entries {
        let Some(score) = pattern.score(entry.haystack.slice(..), &mut matcher) else {
            continue;
        };
        matches.push((
            featured.get(&entry.path).copied(),
            score,
            entry.path.clone(),
            entry.is_dir,
        ));
        if matches.len() >= RANK_BUFFER {
            matches.sort_by(|a, b| compare_file_matches(query, a, b));
            matches.truncate(FILE_SEARCH_MAX_RESULTS);
        }
    }
    matches.sort_by(|a, b| compare_file_matches(query, a, b));
    matches.truncate(FILE_SEARCH_MAX_RESULTS);
    matches
        .into_iter()
        .map(|(_, _, path, is_dir)| FileSearchMatch { path, is_dir })
        .collect()
}

/// Turn a generated chat title into the semantic portion of a Zeron branch
/// (port of zeron's `worktreeBranchFromTitle`). Zeron NFKD-normalizes accented
/// letters first; native keeps it ASCII-only (generated titles are Title Case
/// English), so non-ASCII characters collapse into the `-` separator.
pub fn worktree_branch_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.trim().chars() {
        if matches!(c, '\'' | '"' | '`') {
            continue; // dropped entirely (cafe's → cafes), not a separator
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(48);
    let slug = slug.trim_matches('-');
    format!("zeron/{}", if slug.is_empty() { "update" } else { slug })
}

fn bounded_field(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[derive(serde::Deserialize)]
struct GitHubCommitAvatar {
    sha: String,
    author: Option<GitHubAvatarUser>,
    commit: GitHubCommitMetadata,
}

#[derive(serde::Deserialize)]
struct GitHubAvatarUser {
    avatar_url: String,
}

#[derive(serde::Deserialize)]
struct GitHubCommitMetadata {
    author: GitHubCommitAuthor,
}

#[derive(serde::Deserialize)]
struct GitHubCommitAuthor {
    email: String,
}

fn parse_github_remote(remote: &str) -> Option<(String, String)> {
    let remote = remote.trim();
    let path = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty()
        || repo.is_empty()
        || parts.next().is_some()
        || !owner
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        || !repo.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn parse_history_log(
    output: &str,
    refs_by_sha: &HashMap<String, Vec<GitHistoryRef>>,
) -> Vec<GitHistoryCommit> {
    let fields: Vec<&str> = output.split('\0').collect();
    fields
        .chunks(6)
        .filter_map(|record| {
            if record.len() != 6 {
                return None;
            }
            let sha = record[0].trim_start_matches(['\r', '\n']);
            if sha.is_empty() {
                return None;
            }
            Some(GitHistoryCommit {
                sha: sha.to_string(),
                parent_shas: record[1]
                    .split_ascii_whitespace()
                    .map(str::to_string)
                    .collect(),
                subject: bounded_field(record[2], 4_096),
                author_name: bounded_field(record[3], 512),
                author_email: bounded_field(record[4], 512),
                authored_at: bounded_field(record[5], 128),
                refs: refs_by_sha.get(sha).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_history_refs(output: &str) -> HashMap<String, Vec<GitHistoryRef>> {
    let fields: Vec<&str> = output.split('\0').collect();
    let mut refs_by_sha: HashMap<String, Vec<GitHistoryRef>> = HashMap::new();
    for record in fields.chunks(6) {
        if record.len() != 6 {
            continue;
        }
        let full_name = record[0].trim_start_matches(['\r', '\n']);
        let object_sha = record[1];
        let object_type = record[2];
        let peeled_sha = record[3];
        let peeled_type = record[4];
        let symbolic_target = record[5];
        if full_name.is_empty() || !symbolic_target.is_empty() {
            continue;
        }
        let Some((kind, label)) = (if let Some(label) = full_name.strip_prefix("refs/heads/") {
            Some((GitHistoryRefKind::Branch, label))
        } else if let Some(label) = full_name.strip_prefix("refs/remotes/") {
            Some((GitHistoryRefKind::Remote, label))
        } else {
            full_name
                .strip_prefix("refs/tags/")
                .map(|label| (GitHistoryRefKind::Tag, label))
        }) else {
            continue;
        };
        let target_sha = if object_type == "commit" {
            object_sha
        } else if object_type == "tag" && peeled_type == "commit" {
            peeled_sha
        } else {
            continue;
        };
        refs_by_sha
            .entry(target_sha.to_string())
            .or_default()
            .push(GitHistoryRef {
                kind,
                label: bounded_field(label, 1_024),
            });
    }
    for refs in refs_by_sha.values_mut() {
        refs.sort_by(|left, right| {
            let order = |kind| match kind {
                GitHistoryRefKind::Branch => 0,
                GitHistoryRefKind::Tag => 1,
                GitHistoryRefKind::Remote => 2,
            };
            order(left.kind)
                .cmp(&order(right.kind))
                .then_with(|| left.label.cmp(&right.label))
        });
    }
    refs_by_sha
}

/// Absolute form of a possibly-relative path (no filesystem access).
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_commit(sha: String, parent_sha: Option<String>) -> GitHistoryCommit {
        GitHistoryCommit {
            subject: sha.clone(),
            sha,
            parent_shas: parent_sha.into_iter().collect(),
            author_name: "Test".into(),
            author_email: "test@example.com".into(),
            authored_at: "2026-08-20T12:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    fn score(query: &str, candidate: &str) -> Option<u32> {
        let mut matcher = nucleo_matcher::Matcher::new({
            let mut config = nucleo_matcher::Config::DEFAULT;
            config.set_match_paths();
            config
        });
        nucleo_matcher::pattern::Pattern::parse(
            query,
            nucleo_matcher::pattern::CaseMatching::Smart,
            nucleo_matcher::pattern::Normalization::Smart,
        )
        .score(
            nucleo_matcher::Utf32String::from(candidate).slice(..),
            &mut matcher,
        )
    }

    #[test]
    fn parses_common_github_remote_forms() {
        let expected = Some(("openai".to_string(), "codex".to_string()));
        assert_eq!(
            parse_github_remote("https://github.com/openai/codex.git"),
            expected
        );
        assert_eq!(
            parse_github_remote("git@github.com:openai/codex.git"),
            expected
        );
        assert_eq!(parse_github_remote("https://gitlab.com/openai/codex"), None);
    }

    #[tokio::test]
    async fn github_avatar_downloads_once_into_the_local_cache() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bytes = b"\xff\xd8\xffavatar".to_vec();
        let served = bytes.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        served.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&served).await.unwrap();
        });

        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let url = format!("http://{address}/avatar.jpg");
        let first = repos.cache_github_avatar(&url).await.expect("downloaded");
        server.await.unwrap();
        assert_eq!(tokio::fs::read(&first).await.unwrap(), bytes);
        assert_eq!(
            repos.cache_github_avatar(&url).await.as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn linux_drives_take_media_and_mnt_mounts_system_first() {
        let mounts = "\
sysfs /sys sysfs rw 0 0
/dev/nvme0n1p2 / ext4 rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/sda1 /media/wing/T7\\040Shield exfat rw 0 0
/dev/sdb1 /run/media/wing/Backup ext4 rw 0 0
/dev/sdc1 /mnt/scratch ext4 rw 0 0
/dev/nvme0n1p2 /var/lib/docker ext4 rw 0 0
";
        let drives = finish_drives(linux_drives(mounts));
        let rows: Vec<(&str, &str)> = drives
            .iter()
            .map(|d| (d.name.as_str(), d.path.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("System", "/"),
                ("Backup", "/run/media/wing/Backup"),
                ("scratch", "/mnt/scratch"),
                ("T7 Shield", "/media/wing/T7 Shield"),
            ]
        );
    }

    #[test]
    fn linux_drives_take_custom_top_level_block_mounts_not_system_splits() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
/dev/sdb1 /disk2 ext4 rw 0 0
/dev/mapper/vault /tank btrfs rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
/dev/sdd1 /boot ext4 rw 0 0
/dev/sde1 /home ext4 rw 0 0
/dev/sdf1 /data/disks/a ext4 rw 0 0
/dev/loop3 /snap/core22/1234 squashfs ro 0 0
/dev/loop9 /disk3 ext4 rw 0 0
";
        let drives = finish_drives(linux_drives(mounts));
        let rows: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        // /disk2 and /tank are user drive roots; a loop-mounted ext4 image at
        // a custom root counts too (it's squashfs snaps that are noise). The
        // system's own split partitions and deep mounts stay out.
        assert_eq!(rows, vec!["/", "/disk2", "/disk3", "/tank"]);
    }

    #[test]
    fn finish_drives_dedupes_by_mount_point_keeping_the_first_name() {
        let drives = finish_drives(vec![
            DriveEntry {
                name: "Macintosh HD".into(),
                path: "/".into(),
            },
            DriveEntry {
                name: "System".into(),
                path: "/".into(),
            },
        ]);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].name, "Macintosh HD");
    }

    #[test]
    fn mount_point_unescape_handles_octal_and_lone_backslash() {
        assert_eq!(unescape_mount_point("/media/a\\040b"), "/media/a b");
        assert_eq!(unescape_mount_point("/media/tab\\011x"), "/media/tab\tx");
        assert_eq!(unescape_mount_point("/media/plain"), "/media/plain");
        assert_eq!(unescape_mount_point("/media/tail\\"), "/media/tail\\");
    }

    #[tokio::test]
    async fn list_drives_resolves_on_this_platform() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        // Content is machine-dependent; the call must succeed and dedupe.
        let drives = repos.list_drives().await.unwrap();
        let mut paths: Vec<&String> = drives.iter().map(|d| &d.path).collect();
        paths.dedup();
        assert_eq!(paths.len(), drives.len());
    }

    #[test]
    fn fuzzy_score_matches_a_path_subsequence() {
        assert!(score("cmp rs", "crates/ui/src/composer.rs").is_some());
        assert!(score("composer crates", "crates/ui/src/composer.rs").is_some());
        assert!(score("xyzq", "crates/ui/src/composer.rs").is_none());
    }

    #[test]
    fn git_history_matches_unicode_case_insensitively() {
        let mut candidate = history_commit("a1b2c3d4".into(), None);
        candidate.subject = "RÉPARER la recherche".into();

        assert!(git_history_matches("réparer", &candidate));
    }

    #[test]
    fn history_compaction_handles_a_twenty_thousand_commit_gap() {
        const DEPTH: usize = 20_000;
        let commits = (0..DEPTH)
            .rev()
            .map(|index| {
                history_commit(
                    format!("c{index:05}"),
                    (index > 0).then(|| format!("c{:05}", index - 1)),
                )
            })
            .collect::<Vec<_>>();
        let newest = format!("c{:05}", DEPTH - 1);
        let oldest = "c00000".to_string();
        let visible = HashSet::from([newest.clone(), oldest.clone()]);

        let compact = compact_history_commits(&commits, &visible);

        assert_eq!(compact.len(), 2);
        assert_eq!(compact[0].sha, newest);
        assert_eq!(compact[0].parent_shas, vec![oldest.clone()]);
        assert_eq!(compact[1].sha, oldest);
        assert!(compact[1].parent_shas.is_empty());
    }

    #[test]
    fn search_files_obeys_gitignore_and_returns_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();
        std::fs::write(root.path().join(".secret"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/nope.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "src", &[]).unwrap();
        assert!(
            matches
                .iter()
                .any(|entry| entry.path == "src" && entry.is_dir)
        );
        assert!(matches.iter().any(|entry| entry.path == "src/composer.rs"));
        assert!(
            !matches
                .iter()
                .any(|entry| entry.path.starts_with("ignored"))
        );
        assert!(
            search_files_blocking(root.path(), "secret", &[])
                .unwrap()
                .iter()
                .any(|entry| entry.path == ".secret")
        );
        assert!(!matches.iter().any(|entry| entry.path.starts_with(".git/")));
    }

    #[test]
    fn empty_search_features_recent_paths_before_shallow_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        std::fs::write(root.path().join("src/deep.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "", &["src/deep.rs".into()]).unwrap();
        assert_eq!(
            matches.first().map(|entry| entry.path.as_str()),
            Some("src/deep.rs")
        );
        assert!(matches.iter().any(|entry| entry.path == "README.md"));
    }

    #[test]
    fn search_files_prefers_filename_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("composer/docs")).unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("composer/docs/readme.md"), "").unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "composer", &[]).unwrap();
        let composer = matches
            .iter()
            .position(|entry| entry.path == "src/composer.rs")
            .unwrap();
        let path_only = matches
            .iter()
            .position(|entry| entry.path == "composer/docs/readme.md")
            .unwrap();
        assert!(composer < path_only);
    }

    #[test]
    fn cached_search_reuses_one_walk_within_the_ttl() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "").unwrap();
        let cache: FileIndexCache = std::sync::Mutex::new(HashMap::new());

        let first = search_files_cached(&cache, root.path(), "alpha", &[], || false).unwrap();
        assert_eq!(first.first().map(|m| m.path.as_str()), Some("alpha.rs"));
        std::fs::write(root.path().join("beta.rs"), "").unwrap();
        let second = search_files_cached(&cache, root.path(), "beta", &[], || false).unwrap();
        assert!(second.is_empty(), "{second:?}");
    }

    #[test]
    fn cancelled_search_stops_before_walking() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        let cancelled = AtomicBool::new(true);

        let err = search_files_blocking_with_cancel(root.path(), "", &[], || {
            cancelled.load(Ordering::Relaxed)
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"));
    }

    #[tokio::test]
    async fn workspace_checkout_rejects_sibling_paths() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        assert_eq!(
            repos.workspace_checkout(root.path(), root.path()).await,
            std::fs::canonicalize(root.path()).ok()
        );
        assert!(
            repos
                .workspace_checkout(root.path(), sibling.path())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_searches_of_one_checkout_do_not_cancel_each_other() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "").unwrap();
        std::fs::write(root.path().join("beta.rs"), "").unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        let alpha = repos.search_files(root.path().into(), "alpha".into(), Vec::new());
        let beta = repos.search_files(root.path().into(), "beta".into(), Vec::new());
        let (alpha, beta) = tokio::join!(alpha, beta);

        assert_eq!(alpha.unwrap()[0].path, "alpha.rs");
        assert_eq!(beta.unwrap()[0].path, "beta.rs");
    }
}
