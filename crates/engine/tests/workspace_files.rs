//! End-to-end coverage for the workspace file RPC surface over the real
//! in-memory transport.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::{
    WorkspaceDirectoryPage, WorkspaceFileChanges, WorkspaceFileText, WriteWorkspaceFileOutcome,
};
use zeron_rpc::methods;

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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn init_repo(path: &Path) {
    std::fs::create_dir_all(path.join("src/nested")).expect("repo tree");
    git(path, &["init", "-b", "main"]).await;
    std::fs::write(path.join("README.md"), "hello\n").expect("readme");
    std::fs::write(path.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").expect("source");
    std::fs::write(path.join("src/nested/mod.rs"), "pub mod child;\n").expect("nested source");
    git(path, &["add", "."]).await;
    git(path, &["commit", "-m", "initial"]).await;
}

fn assemble(data_dir: &Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(data_dir).expect("data dir");
    std::fs::write(data_dir.join("device-id"), device_id).expect("device id");
    EngineCore::assemble(
        data_dir,
        Arc::new(HarnessRegistry::new()),
        zeron_proto::HarnessId::Mock,
        None,
    )
    .expect("engine assembles")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_file_rpcs_list_search_read_write_and_watch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    init_repo(&repo).await;
    let core = assemble(&temp.path().join("data"), "device-files");
    core.workspace
        .create_space(
            "space-files",
            &core.device_id,
            &repo.to_string_lossy(),
            None,
            true,
        )
        .expect("space");
    core.workspace
        .create_chat("chat-files", Some("space-files"), None, None, None)
        .expect("chat");
    let client = zeron_rpc::memory_client(core.rpc_service());

    let root = client
        .call(
            methods::LIST_WORKSPACE_DIRECTORY,
            serde_json::json!({ "chatId": "chat-files" }),
        )
        .await
        .expect("list root");
    let root: WorkspaceDirectoryPage = serde_json::from_value(root).expect("typed root");
    assert_eq!(root.entries[0].path, "src");
    assert!(root.entries.iter().any(|entry| entry.path == "README.md"));
    assert!(!root.entries.iter().any(|entry| entry.path == ".git"));

    let nested = client
        .call(
            methods::LIST_WORKSPACE_DIRECTORY,
            serde_json::json!({ "chatId": "chat-files", "directory": "src" }),
        )
        .await
        .expect("list src");
    let nested: WorkspaceDirectoryPage = serde_json::from_value(nested).expect("typed src");
    assert!(
        nested
            .entries
            .iter()
            .any(|entry| entry.path == "src/lib.rs")
    );

    let matches = client
        .call(
            methods::SEARCH_WORKSPACE_FILES,
            serde_json::json!({ "chatId": "chat-files", "query": "lib" }),
        )
        .await
        .expect("search");
    assert_eq!(matches[0]["path"], "src/lib.rs");

    let read = client
        .call(
            methods::READ_WORKSPACE_FILE,
            serde_json::json!({ "chatId": "chat-files", "path": "README.md" }),
        )
        .await
        .expect("read");
    let read: WorkspaceFileText = serde_json::from_value(read).expect("typed read");
    assert_eq!(read.text.as_deref(), Some("hello\n"));
    let original_hash = read.content_hash.expect("content hash");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><rect width="20" height="20"/></svg>"#;
    std::fs::write(repo.join("example.svg"), svg).unwrap();
    let image = client.call(methods::READ_WORKSPACE_IMAGE, serde_json::json!({ "chatId": "chat-files", "path": "example.svg", "expectedCheckoutId": read.checkout_id, "offset": 0 })).await.expect("workspace image");
    assert_eq!(image["mimeType"], "image/svg+xml");
    assert_eq!(image["done"], true);
    assert_eq!(image["size"], svg.len());
    for (path, checkout) in [
        ("example.svg", "wrong-checkout"),
        ("../example.svg", read.checkout_id.as_str()),
        (".git/config", read.checkout_id.as_str()),
    ] {
        assert!(client.call(methods::READ_WORKSPACE_IMAGE, serde_json::json!({ "chatId": "chat-files", "path": path, "expectedCheckoutId": checkout, "offset": 0 })).await.is_err());
    }

    let mut watch = client
        .subscribe(
            methods::WATCH_WORKSPACE_FILES,
            serde_json::json!({ "chatId": "chat-files" }),
        )
        .await
        .expect("watch subscribe");
    let baseline = tokio::time::timeout(Duration::from_secs(3), watch.recv())
        .await
        .expect("baseline timeout")
        .expect("watch alive");
    let baseline: WorkspaceFileChanges = serde_json::from_value(baseline).expect("baseline shape");
    assert!(baseline.resync_required);

    let written = client
        .call(
            methods::WRITE_WORKSPACE_FILE,
            serde_json::json!({
                "chatId": "chat-files",
                "path": "README.md",
                "text": "updated\n",
                "expectedCheckoutId": read.checkout_id,
                "expectedContentHash": original_hash,
                "encoding": "utf8",
                "lineEnding": "lf",
            }),
        )
        .await
        .expect("write");
    let written: WriteWorkspaceFileOutcome =
        serde_json::from_value(written).expect("written shape");
    let new_hash = match written {
        WriteWorkspaceFileOutcome::Written { file } => file.content_hash,
        outcome => panic!("unexpected write outcome: {outcome:?}"),
    };
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "updated\n"
    );

    let conflict = client
        .call(
            methods::WRITE_WORKSPACE_FILE,
            serde_json::json!({
                "chatId": "chat-files",
                "path": "README.md",
                "text": "stale overwrite\n",
                "expectedCheckoutId": read.checkout_id,
                "expectedContentHash": "stale",
                "encoding": "utf8",
                "lineEnding": "lf",
            }),
        )
        .await
        .expect("conflict response");
    assert_eq!(conflict["status"], "conflict");
    assert_eq!(conflict["currentContentHash"], new_hash);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "updated\n"
    );

    let concurrent_read = client
        .call(
            methods::READ_WORKSPACE_FILE,
            serde_json::json!({ "chatId": "chat-files", "path": "README.md" }),
        )
        .await
        .expect("read before concurrent writers");
    let concurrent_hash = concurrent_read["contentHash"]
        .as_str()
        .expect("concurrent hash")
        .to_string();
    let first_write = client.call(
        methods::WRITE_WORKSPACE_FILE,
        serde_json::json!({
            "chatId": "chat-files",
            "path": "README.md",
            "text": "concurrent\n",
            "expectedCheckoutId": read.checkout_id,
                "expectedContentHash": concurrent_hash.clone(),
            "encoding": "utf8",
            "lineEnding": "lf",
        }),
    );
    let second_write = client.call(
        methods::WRITE_WORKSPACE_FILE,
        serde_json::json!({
            "chatId": "chat-files",
            "path": "README.md",
            "text": "concurrent\n",
            "expectedCheckoutId": read.checkout_id,
                "expectedContentHash": concurrent_hash,
            "encoding": "utf8",
            "lineEnding": "lf",
        }),
    );
    let (first_write, second_write) = tokio::join!(first_write, second_write);
    let statuses = [
        first_write.expect("first concurrent writer")["status"]
            .as_str()
            .expect("first status")
            .to_string(),
        second_write.expect("second concurrent writer")["status"]
            .as_str()
            .expect("second status")
            .to_string(),
    ];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| status.as_str() == "written")
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| status.as_str() == "conflict")
            .count(),
        1
    );

    std::fs::write(repo.join("created.txt"), "created\n").expect("external create");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let item = tokio::time::timeout_at(deadline, watch.recv())
            .await
            .expect("watch change timeout")
            .expect("watch alive");
        let item: WorkspaceFileChanges = serde_json::from_value(item).expect("change shape");
        if item
            .changes
            .iter()
            .any(|change| change.path == "created.txt")
        {
            break;
        }
    }

    assert!(
        client
            .call(
                methods::READ_WORKSPACE_FILE,
                serde_json::json!({ "chatId": "chat-files", "path": "../outside" }),
            )
            .await
            .is_err()
    );
    drop(watch);
    core.shutdown().await;
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_file_rpcs_preserve_plain_folder_search_support() {
    let temp = tempfile::tempdir().expect("tempdir");
    let folder = temp.path().join("plain-folder");
    std::fs::create_dir_all(&folder).expect("plain folder");
    std::fs::write(folder.join("notes.txt"), "plain\n").expect("plain file");
    let core = assemble(&temp.path().join("plain-data"), "device-plain");
    core.workspace
        .create_space(
            "space-plain",
            &core.device_id,
            &folder.to_string_lossy(),
            None,
            false,
        )
        .expect("plain space");
    let client = zeron_rpc::memory_client(core.rpc_service());

    let legacy = client
        .call(
            methods::SEARCH_FILES,
            serde_json::json!({ "spaceId": "space-plain", "query": "notes" }),
        )
        .await
        .expect("legacy SearchFiles on plain folder");
    assert_eq!(legacy[0]["path"], "notes.txt");
    let listing = client
        .call(
            methods::LIST_WORKSPACE_DIRECTORY,
            serde_json::json!({ "spaceId": "space-plain" }),
        )
        .await
        .expect("workspace listing on plain folder");
    assert!(
        listing["entries"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry["path"] == "notes.txt"))
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_rejects_changed_checkout_even_when_contents_match() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let worktree = temp.path().join("worktree");
    init_repo(&repo).await;
    git(
        &repo,
        &["worktree", "add", "-b", "other", worktree.to_str().unwrap()],
    )
    .await;
    let core = assemble(&temp.path().join("data"), "device-files");
    core.workspace
        .create_space(
            "space-files",
            &core.device_id,
            repo.to_str().unwrap(),
            None,
            true,
        )
        .unwrap();
    core.workspace
        .create_chat("chat-files", Some("space-files"), None, None, None)
        .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let read = client
        .call(
            methods::READ_WORKSPACE_FILE,
            serde_json::json!({
                "chatId": "chat-files", "path": "README.md"
            }),
        )
        .await
        .unwrap();
    assert!(!read["checkoutId"].as_str().unwrap().is_empty());
    core.workspace
        .set_chat_cwd("chat-files", worktree.to_str().unwrap())
        .unwrap();
    let other = client
        .call(
            methods::READ_WORKSPACE_FILE,
            serde_json::json!({
                "chatId": "chat-files", "path": "README.md"
            }),
        )
        .await
        .unwrap();
    assert_eq!(read["contentHash"], other["contentHash"]);
    assert_ne!(read["checkoutId"], other["checkoutId"]);
    let mut request = serde_json::json!({
        "chatId": "chat-files", "path": "README.md", "text": "pending original edit\n",
        "expectedContentHash": read["contentHash"], "expectedCheckoutId": read["checkoutId"],
        "encoding": "utf8", "lineEnding": "lf"
    });
    let error = client
        .call(methods::WRITE_WORKSPACE_FILE, request.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Workspace changed"), "{error}");
    request
        .as_object_mut()
        .unwrap()
        .remove("expectedCheckoutId");
    assert!(
        client
            .call(methods::WRITE_WORKSPACE_FILE, request.clone())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("README.md")).unwrap(),
        "hello\n"
    );
    // Returning to the original checkout allows the preserved edit to be saved.
    core.workspace
        .set_chat_cwd("chat-files", repo.to_str().unwrap())
        .unwrap();
    request["expectedCheckoutId"] = read["checkoutId"].clone();
    let written = client
        .call(methods::WRITE_WORKSPACE_FILE, request)
        .await
        .unwrap();
    assert_eq!(written["status"], "written");
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "pending original edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("README.md")).unwrap(),
        "hello\n"
    );
}
