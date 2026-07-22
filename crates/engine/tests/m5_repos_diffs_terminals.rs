//! M5a integration: repos/worktrees, folder listing, checkout-diff capture + sync,
//! terminals, and the RPC dispatch for each new method over the memory transport.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use comet_engine::{EngineCore, HarnessRegistry, Repos, Terminals, capture_diff};
use comet_proto::TerminalEvent;
use comet_rpc::methods;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn git(cwd: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .await
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Init a repo at `dir` with one committed file `a.txt`.
async fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-b", "main"]).await;
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").expect("write a.txt");
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-m", "initial"]).await;
}

fn test_repos(data_dir: &Path) -> Repos {
    Repos::with_worktrees_root(data_dir, "device-test", data_dir.join("worktrees"))
}

fn assemble(dir: &Path) -> EngineCore {
    std::fs::create_dir_all(dir).expect("data dir");
    EngineCore::assemble(
        dir,
        Arc::new(HarnessRegistry::new()),
        comet_proto::HarnessId::Mock,
        None,
    )
    .expect("engine assembles")
}

fn decoded(events: &[TerminalEvent]) -> String {
    let mut out = Vec::new();
    for event in events {
        if let TerminalEvent::Data { data, .. } = event {
            out.extend(BASE64.decode(data).expect("valid base64"));
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Drain a terminal subscription until `predicate` matches the decoded transcript
/// (or the deadline hits).
async fn drain_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<TerminalEvent>,
    events: &mut Vec<TerminalEvent>,
    predicate: impl Fn(&[TerminalEvent]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !predicate(events) {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("terminal event before timeout")
            .expect("terminal stream alive");
        events.push(event);
    }
}

// ---------------------------------------------------------------------------
// Repos
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repos_round_trip_add_branches_worktrees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("myrepo");
    init_repo(&repo_dir).await;
    let repos = test_repos(&tmp.path().join("data"));

    // Add + list.
    let repo = repos
        .add(&repo_dir.to_string_lossy())
        .await
        .expect("add repo");
    assert_eq!(repo.name, "myrepo");
    assert_eq!(repo.default_branch.as_deref(), Some("main"));
    let listed = repos.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, repo_dir.to_string_lossy());

    // Re-add dedupes; junk paths fail.
    repos
        .add(&repo_dir.to_string_lossy())
        .await
        .expect("re-add repo");
    assert_eq!(repos.list().await.len(), 1);
    assert!(repos.add("/definitely/not/a/path").await.is_err());
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).expect("plain dir");
    assert!(
        repos.add(&plain.to_string_lossy()).await.is_err(),
        "non-repo dir rejected"
    );

    // Branch listing: default branch first.
    git(&repo_dir, &["branch", "feature/x"]).await;
    let branches = repos.branches(&repo_dir).await.expect("branches");
    assert_eq!(branches[0], "main", "default branch first: {branches:?}");
    assert!(branches.contains(&"feature/x".to_string()));

    // Worktree add: comet/<name> branch, isolated dir under the test root.
    let worktree = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");
    assert!(
        worktree.branch.starts_with("comet/"),
        "branch: {}",
        worktree.branch
    );
    assert!(PathBuf::from(&worktree.path).join("a.txt").exists());
    assert!(worktree.checkout_id.is_some());
    assert!(
        worktree
            .path
            .starts_with(&*tmp.path().join("data").to_string_lossy())
    );
    let branches = repos
        .branches(&repo_dir)
        .await
        .expect("branches after worktree");
    assert!(branches.contains(&worktree.branch));

    // Worktree checkout identity differs from the main checkout's.
    let main_identity = repos
        .checkout_identity(&repo_dir)
        .await
        .expect("main identity");
    let wt_identity = repos
        .checkout_identity(Path::new(&worktree.path))
        .await
        .expect("wt identity");
    assert_ne!(main_identity.id, wt_identity.id);

    // Delete: dir removed, comet branch removed, refs pruned.
    repos
        .delete_worktree(&repo_dir, Path::new(&worktree.path))
        .await
        .expect("delete worktree");
    assert!(!PathBuf::from(&worktree.path).exists());
    let branches = repos
        .branches(&repo_dir)
        .await
        .expect("branches after delete");
    assert!(
        !branches.contains(&worktree.branch),
        "comet branch deleted: {branches:?}"
    );

    // CreateRepo: sanitized name, initialized on main.
    let created = repos.create("demo repo!").await.expect("create repo");
    assert_eq!(created.name, "demo-repo-");
    assert!(PathBuf::from(&created.path).join(".git").exists());
    assert!(
        repos.create("demo repo!").await.is_err(),
        "duplicate create rejected"
    );
}

// ---------------------------------------------------------------------------
// Folder lister
// ---------------------------------------------------------------------------

#[tokio::test]
async fn folder_lister_flags_and_ordering() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("beta/.git")).expect("repo-ish dir");
    std::fs::create_dir_all(tmp.path().join("alpha")).expect("dir");
    std::fs::create_dir_all(tmp.path().join(".hidden")).expect("hidden dir");
    std::fs::write(tmp.path().join("aaa.txt"), "x").expect("file");

    // Data dir OUTSIDE the listed directory so the fixture stays exact.
    let data = tempfile::tempdir().expect("data dir");
    let repos = test_repos(data.path());
    let listing = repos
        .list_folders(Some(tmp.path().to_string_lossy().to_string()))
        .await
        .expect("listing");
    assert!(!listing.truncated);
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    // Dirs first (name-sorted), files after; dotfiles hidden.
    assert_eq!(names, vec!["alpha", "beta", "aaa.txt"]);
    let beta = listing
        .entries
        .iter()
        .find(|e| e.name == "beta")
        .expect("beta entry");
    assert!(beta.is_dir && beta.is_repo);
    let alpha = listing
        .entries
        .iter()
        .find(|e| e.name == "alpha")
        .expect("alpha entry");
    assert!(alpha.is_dir && !alpha.is_repo);
    let file = listing
        .entries
        .iter()
        .find(|e| e.name == "aaa.txt")
        .expect("file entry");
    assert!(!file.is_dir);
}

#[tokio::test]
async fn folder_lister_caps_at_500_with_truncated_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for i in 0..510 {
        std::fs::create_dir_all(tmp.path().join(format!("dir-{i:04}"))).expect("dir");
    }
    let data = tempfile::tempdir().expect("data dir");
    let repos = test_repos(data.path());
    let listing = repos
        .list_folders(Some(tmp.path().to_string_lossy().to_string()))
        .await
        .expect("listing");
    assert_eq!(listing.entries.len(), 500);
    assert!(listing.truncated);
}

#[tokio::test]
async fn folder_lister_timeout_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repos = test_repos(&tmp.path().join("data"));
    let err = repos
        .list_folders_with(
            Some(tmp.path().to_string_lossy().to_string()),
            Duration::from_millis(50),
            true, // worker never responds
        )
        .await
        .expect_err("times out");
    assert!(
        err.to_string().contains("timed out"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Diff capture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diff_capture_tracked_untracked_and_checksum() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = test_repos(&tmp.path().join("data"));

    // Clean tree: empty patch, no files, stable checksum.
    let clean = capture_diff(&repos, &repo_dir)
        .await
        .expect("clean capture");
    assert!(clean.patch.is_empty());
    assert!(clean.files.is_empty());
    assert!(!clean.truncated);
    assert_eq!(clean.branch, "main");
    assert!(clean.head_sha.is_some());

    // Modify tracked + add untracked.
    std::fs::write(repo_dir.join("a.txt"), "one\nTWO\nthree\n").expect("modify a.txt");
    std::fs::write(repo_dir.join("b.txt"), "brand new\nline two\n").expect("untracked b.txt");
    let snapshot = capture_diff(&repos, &repo_dir).await.expect("capture");
    assert!(snapshot.patch.contains("a/a.txt"), "tracked diff present");
    assert!(snapshot.patch.contains("+TWO"));
    assert!(
        snapshot.patch.contains("diff --git a/b.txt b/b.txt"),
        "untracked new-file hunk"
    );
    assert!(snapshot.patch.contains("+brand new"));
    assert!(!snapshot.truncated);
    let a = snapshot
        .files
        .iter()
        .find(|f| f.path == "a.txt")
        .expect("a.txt summary");
    assert_eq!(a.status, "modified");
    assert!(
        a.additions >= 2 && a.deletions >= 1,
        "numstat applied: {a:?}"
    );
    let b = snapshot
        .files
        .iter()
        .find(|f| f.path == "b.txt")
        .expect("b.txt summary");
    assert_eq!(b.status, "added");
    assert_eq!(b.additions, 2);
    assert!(snapshot.additions >= 4);

    // Checksum: stable across identical captures, changed by any edit.
    let again = capture_diff(&repos, &repo_dir).await.expect("recapture");
    assert_eq!(
        snapshot.checksum, again.checksum,
        "checksum stable when nothing changed"
    );
    std::fs::write(repo_dir.join("b.txt"), "different\n").expect("edit b.txt");
    let changed = capture_diff(&repos, &repo_dir)
        .await
        .expect("changed capture");
    assert_ne!(snapshot.checksum, changed.checksum);
}

#[tokio::test]
async fn diff_capture_truncates_at_patch_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = test_repos(&tmp.path().join("data"));

    // Rewrite the tracked file with >3MiB of fresh lines: the tracked patch blows
    // through MAX_PATCH_BYTES and must come back truncated with the marker.
    let mut big = String::with_capacity(4 * 1024 * 1024 + 16);
    for i in 0..200_000 {
        big.push_str(&format!("line number {i} padded\n"));
    }
    std::fs::write(repo_dir.join("a.txt"), &big).expect("big rewrite");
    let snapshot = capture_diff(&repos, &repo_dir).await.expect("capture");
    assert!(snapshot.truncated, "patch cap hit");
    assert!(snapshot.patch.len() <= 3 * 1024 * 1024 + 64);
    assert!(snapshot.patch.contains("# Comet diff truncated"));
}

// ---------------------------------------------------------------------------
// Spaces sync (git presence stamping + orphan sweep) via EngineCore
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spaces_sync_stamps_git_presence_and_reacts_to_git_init() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let folder = tmp.path().join("plain-folder");
    std::fs::create_dir_all(&folder).expect("folder");

    let core = assemble(&tmp.path().join("data"));
    // Seeded as git (a lying picker) — the owner's sync must correct it.
    core.workspace
        .create_space("space-1", &core.device_id, &folder.to_string_lossy(), None, true)
        .expect("space row");
    core.spaces_sync.reconcile_now().await;

    let mut spaces_rx = core.workspace.watch_spaces();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let space = loop {
        {
            let spaces = spaces_rx.borrow().clone();
            if let Some(space) = spaces
                .iter()
                .find(|s| s.id == "space-1" && s.git_checked_at.is_some())
            {
                break space.clone();
            }
        }
        tokio::time::timeout_at(deadline, spaces_rx.changed())
            .await
            .expect("git check before timeout")
            .expect("watch alive");
    };
    assert!(!space.git_detected, "plain folder must read as non-git");
    assert!(space.checkout_id.is_none());

    // `git init` later flips the stamp (watcher and/or explicit recheck).
    git(&folder, &["init", "-b", "main"]).await;
    core.spaces_sync.reconcile_now().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let space = loop {
        {
            let spaces = spaces_rx.borrow().clone();
            if let Some(space) = spaces.iter().find(|s| s.id == "space-1" && s.git_detected) {
                break space.clone();
            }
        }
        tokio::time::timeout_at(deadline, spaces_rx.changed())
            .await
            .expect("git init detected before timeout")
            .expect("watch alive");
    };
    assert!(space.checkout_id.is_some(), "git space gains a checkout id");
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_space_cascades_chats_and_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let folder = tmp.path().join("folder");
    std::fs::create_dir_all(&folder).expect("folder");

    let core = assemble(&tmp.path().join("data"));
    core.workspace
        .create_space("space-1", &core.device_id, &folder.to_string_lossy(), None, false)
        .expect("space row");
    core.workspace
        .create_chat("chat-1", "space-1", None, None)
        .expect("chat row");
    let chat = core
        .workspace
        .doc()
        .chat("chat-1")
        .expect("read")
        .expect("exists");
    assert_eq!(chat.space_id.as_deref(), Some("space-1"));
    assert_eq!(chat.device_id, core.device_id);
    assert_eq!(chat.cwd.as_deref(), Some(&*folder.to_string_lossy()));

    let deleted = core.workspace.delete_space("space-1").expect("cascade");
    assert!(deleted.existed);
    assert_eq!(deleted.chat_ids, vec!["chat-1".to_string()]);
    assert!(core.workspace.doc().chat("chat-1").expect("read").is_none());
    assert!(core.workspace.read_spaces().expect("spaces").is_empty());
    core.shutdown().await;
}

// ---------------------------------------------------------------------------
// Diff sync (watchers + workspace branch upkeep) via EngineCore
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_sync_publishes_and_updates_chat_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    std::fs::write(repo_dir.join("a.txt"), "one\ntwo\nedited\n").expect("dirty tree");

    let core = assemble(&tmp.path().join("data"));
    core.workspace
        .create_space(
            "space-diff",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("space row");
    core.workspace
        .create_chat("chat-diff", "space-diff", None, None)
        .expect("chat row");
    core.diff_sync.reconcile_now().await;

    // Initial snapshot lands after the debounce; poll the watch.
    let mut diffs_rx = core.diff_sync.watch_diffs();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let diff = loop {
        {
            let diffs = diffs_rx.borrow().clone();
            if let Some(diff) = diffs.first() {
                break diff.clone();
            }
        }
        tokio::time::timeout_at(deadline, diffs_rx.changed())
            .await
            .expect("diff published before timeout")
            .expect("watch alive");
    };
    assert_eq!(diff.device_id, core.device_id);
    assert!(diff.patch.contains("+edited"));
    assert!(!diff.checkout_id.is_empty());
    assert!(!diff.checksum.is_empty());

    // Row upkeep: branch + checkoutId stamped on the workspace chat row.
    let chat = core
        .workspace
        .doc()
        .chat("chat-diff")
        .expect("read chat")
        .expect("row");
    assert_eq!(chat.branch.as_deref(), Some("main"));
    assert_eq!(chat.checkout_id.as_deref(), Some(diff.checkout_id.as_str()));

    // File watcher path: another edit re-publishes without a manual kick.
    let before = diff.checksum.clone();
    std::fs::write(repo_dir.join("watched.txt"), "fresh untracked\n").expect("new file");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        {
            let diffs = diffs_rx.borrow().clone();
            if let Some(diff) = diffs.first()
                && diff.checksum != before
            {
                assert!(diff.patch.contains("watched.txt"));
                break;
            }
        }
        tokio::time::timeout_at(deadline, diffs_rx.changed())
            .await
            .expect("watcher-driven publish before timeout")
            .expect("watch alive");
    }
    core.shutdown().await;
}

// ---------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_e2e_replay_live_resize_exit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let terminals = Terminals::new();
    let session = terminals
        .open_with_shell(&tmp.path().to_string_lossy(), 80, 24, Some("/bin/sh"))
        .expect("open sh");
    assert_eq!(session.shell, "sh");
    assert_eq!(session.cwd, tmp.path().to_string_lossy());

    // Live subscribe, then run a command whose OUTPUT differs from the echoed input.
    let mut rx = terminals.subscribe(&session.id, None).expect("subscribe");
    let mut events = Vec::new();
    terminals
        .write(&session.id, &BASE64.encode("echo m4rk3r-$((40+2))\n"))
        .expect("write echo");
    drain_until(&mut rx, &mut events, |events| {
        decoded(events).contains("m4rk3r-42")
    })
    .await;

    // Resize is accepted (values clamped internally).
    terminals.resize(&session.id, 132, 40).expect("resize");
    terminals
        .resize(&session.id, 1, 1000)
        .expect("clamped resize");

    // Detach (drop the stream) — the shell survives and keeps producing output.
    drop(rx);
    terminals
        .write(&session.id, &BASE64.encode("echo aft3r-$((10+1))\n"))
        .expect("write");
    let mut rx2 = terminals
        .subscribe(&session.id, None)
        .expect("re-subscribe");
    let mut events2 = Vec::new();
    drain_until(&mut rx2, &mut events2, |events| {
        let text = decoded(events);
        text.contains("m4rk3r-42") && text.contains("aft3r-11")
    })
    .await;
    // The replay was re-delivered from seq 0 — first event seq is 1.
    let first_seq = match events2.first().expect("replayed events") {
        TerminalEvent::Data { seq, .. } | TerminalEvent::Exit { seq, .. } => *seq,
    };
    assert_eq!(first_seq, 1);

    // afterSeq resume skips already-seen events.
    let last_seen = match events2.last().expect("events") {
        TerminalEvent::Data { seq, .. } | TerminalEvent::Exit { seq, .. } => *seq,
    };
    let mut rx3 = terminals
        .subscribe(&session.id, Some(last_seen))
        .expect("resume");

    // Exit: shell terminates, Exit event lands on every live stream, streams end.
    terminals
        .write(&session.id, &BASE64.encode("exit 3\n"))
        .expect("write exit");
    let mut events3 = Vec::new();
    drain_until(&mut rx3, &mut events3, |events| {
        events
            .iter()
            .any(|e| matches!(e, TerminalEvent::Exit { .. }))
    })
    .await;
    match events3.last().expect("exit event") {
        TerminalEvent::Exit { exit_code, .. } => assert_eq!(*exit_code, 3),
        other => panic!("expected exit last, got {other:?}"),
    }
    assert!(rx3.recv().await.is_none(), "stream ends after exit");

    // Exited-session replay: subscribe again → full replay then immediate end.
    let mut rx4 = terminals
        .subscribe(&session.id, None)
        .expect("post-exit subscribe");
    let mut events4 = Vec::new();
    while let Some(event) = rx4.recv().await {
        events4.push(event);
    }
    assert!(decoded(&events4).contains("m4rk3r-42"));
    assert!(matches!(
        events4.last(),
        Some(TerminalEvent::Exit { exit_code: 3, .. })
    ));

    // Writes to an exited terminal fail; close removes it entirely.
    assert!(
        terminals
            .write(&session.id, &BASE64.encode("nope\n"))
            .is_err()
    );
    terminals.close(&session.id).expect("close");
    assert!(
        terminals.subscribe(&session.id, None).is_err(),
        "closed terminal is gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_guards_input_size_and_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let terminals = Terminals::new();
    assert!(
        terminals
            .open_with_shell("/definitely/not/a/dir", 80, 24, Some("/bin/sh"))
            .is_err(),
        "bad cwd rejected"
    );
    let session = terminals
        .open_with_shell(&tmp.path().to_string_lossy(), 80, 24, Some("/bin/sh"))
        .expect("open sh");
    let huge = BASE64.encode(vec![b'x'; 65 * 1024]);
    assert!(
        terminals.write(&session.id, &huge).is_err(),
        "oversized input rejected"
    );
    terminals.close(&session.id).expect("close");
}

// ---------------------------------------------------------------------------
// RPC dispatch over the in-memory transport
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_dispatch_for_m5_methods() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // EngineCore's Repos resolves the worktree root from the env; keep test
    // worktrees out of $HOME. (Process-global — this is the only test that sets it.)
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", tmp.path().join("worktrees")) };
    let core = assemble(&tmp.path().join("data"));
    let client = comet_rpc::memory_client(core.rpc_service());

    // CreateRepo → ListRepos.
    let created = client
        .call(methods::CREATE_REPO, serde_json::json!({ "name": "demo" }))
        .await
        .expect("CreateRepo");
    assert_eq!(created["name"], "demo");
    let repo_path = created["path"].as_str().expect("repo path").to_string();
    let listed = client
        .call(methods::LIST_REPOS, serde_json::Value::Null)
        .await
        .expect("ListRepos");
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    // AddRepo (idempotent re-add of the same path).
    let added = client
        .call(methods::ADD_REPO, serde_json::json!({ "path": repo_path }))
        .await
        .expect("AddRepo");
    assert_eq!(added["name"], "demo");

    // Seed a commit so branches/worktrees exist.
    let repo_dir = PathBuf::from(&repo_path);
    std::fs::write(repo_dir.join("file.txt"), "hello\n").expect("seed file");
    git(&repo_dir, &["add", "."]).await;
    git(&repo_dir, &["commit", "-m", "seed"]).await;

    // ListBranches: default (checked-out) branch first.
    let branches = client
        .call(
            methods::LIST_BRANCHES,
            serde_json::json!({ "repoPath": repo_path }),
        )
        .await
        .expect("ListBranches");
    assert_eq!(branches[0], "main");

    // ListFolders with an explicit path.
    let folders = client
        .call(
            methods::LIST_FOLDERS,
            serde_json::json!({ "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("ListFolders");
    assert!(folders["entries"].as_array().is_some());
    assert_eq!(
        folders["path"].as_str(),
        Some(&*tmp.path().to_string_lossy())
    );

    // CreateWorktree / DeleteWorktree.
    let worktree = client
        .call(
            methods::CREATE_WORKTREE,
            serde_json::json!({ "repoPath": repo_path, "branch": "main" }),
        )
        .await
        .expect("CreateWorktree");
    let worktree_path = worktree["path"]
        .as_str()
        .expect("worktree path")
        .to_string();
    assert!(
        worktree["branch"]
            .as_str()
            .expect("branch")
            .starts_with("comet/")
    );
    assert!(worktree["checkoutId"].is_string());
    let deleted = client
        .call(
            methods::DELETE_WORKTREE,
            serde_json::json!({ "repoPath": repo_path, "worktreePath": worktree_path }),
        )
        .await
        .expect("DeleteWorktree");
    assert_eq!(deleted["ok"], true);
    assert!(!PathBuf::from(&worktree_path).exists());

    // WatchCheckoutDiffs: streams the current (empty) diff set immediately.
    let mut diffs_stream = client
        .subscribe(methods::WATCH_CHECKOUT_DIFFS, serde_json::Value::Null)
        .await
        .expect("WatchCheckoutDiffs");
    let first = tokio::time::timeout(Duration::from_secs(5), diffs_stream.recv())
        .await
        .expect("first diffs item")
        .expect("stream alive");
    assert!(first.is_array());

    // Terminals: the chat's cwd (via its space) becomes the PTY cwd.
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace",
                "spaceId": "space-term",
                "deviceId": core.device_id,
                "path": repo_path,
                "gitDetected": true,
            }),
        )
        .await
        .expect("createSpace");
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "chat-term",
                "spaceId": "space-term",
            }),
        )
        .await
        .expect("createChat");
    let session = client
        .call(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "chatId": "chat-term", "cols": 80, "rows": 24 }),
        )
        .await
        .expect("OpenTerminal");
    let terminal_id = session["id"].as_str().expect("terminal id").to_string();
    assert_eq!(session["cwd"], repo_path);

    let mut stream = client
        .subscribe(
            methods::SUBSCRIBE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id }),
        )
        .await
        .expect("SubscribeTerminal");
    client
        .call(
            methods::WRITE_TERMINAL,
            serde_json::json!({
                "terminalId": terminal_id,
                "data": BASE64.encode("echo rpc-t3st-$((5+4))\n"),
            }),
        )
        .await
        .expect("WriteTerminal");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut transcript = Vec::new();
    loop {
        let item = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .expect("terminal output before timeout")
            .expect("stream alive");
        if item["type"] == "data" {
            let bytes = BASE64
                .decode(item["data"].as_str().expect("data"))
                .expect("valid base64");
            transcript.extend(bytes);
        }
        if String::from_utf8_lossy(&transcript).contains("rpc-t3st-9") {
            break;
        }
    }
    let resized = client
        .call(
            methods::RESIZE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id, "cols": 132, "rows": 43 }),
        )
        .await
        .expect("ResizeTerminal");
    assert_eq!(resized["ok"], true);
    let closed = client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id }),
        )
        .await
        .expect("CloseTerminal");
    assert_eq!(closed["ok"], true);
    let err = client
        .call(
            methods::WRITE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id, "data": "eA==" }),
        )
        .await
        .expect_err("closed terminal rejects writes");
    assert!(
        err.to_string().contains("not found"),
        "unexpected error: {err}"
    );

    core.shutdown().await;
}
