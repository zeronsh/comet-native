//! Host-side worktree materialization: a Run command carrying a
//! `WorktreeSpec` creates the isolated worktree on the HOST at drain time
//! (the durable replacement for the composer's old blocking CreateWorktree
//! relay RPC), runs there, and stamps the chat row's cwd + `zeron/<name>`
//! branch. A second spec-carrying Run for the same chat REUSES the checkout
//! instead of minting another.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{MessageRole, MessageStatus, SessionCommandPayload, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ProjectActionDraft, ProjectActionIcon,
    ReasoningLevel, RunRequest, SandboxLevel, SteeringMode, WorktreeSpec,
};

const CHAT: &str = "chat-worktree-run";

/// Completes a one-line turn and records the cwd each run spawned with.
struct RecordingHarness {
    cwds: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recorder"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.cwds.lock().unwrap().push(request.cwd.clone());
        let events: Vec<Result<AgentEvent, HarnessError>> = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess-wt".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: format!("ack: {}", request.prompt),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-wt".into()),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    let entries: Vec<SessionMessageEntry> = core
        .doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn run_payload(message_id: &str, repo_path: &str, space_id: Option<&str>) -> SessionCommandPayload {
    SessionCommandPayload::Run {
        request: RunRequest {
            prompt: "isolated please".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            // Fallback for hosts that predate the spec: the repo's own folder.
            cwd: repo_path.into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
            worktree: Some(WorktreeSpec {
                repo_path: repo_path.into(),
                base: "main".into(),
                space_id: space_id.map(str::to_string),
            }),
        },
        message_id: message_id.into(),
    }
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_with_worktree_spec_materializes_on_host_and_reuses() {
    let tmp = tempfile::tempdir().unwrap();
    // Canonicalize: git records canonical paths in worktree gitdir links, and
    // macOS tempdirs live behind the /var → /private/var symlink.
    let tmp_path = tmp.path().canonicalize().unwrap();
    let worktrees_root = tmp_path.join("worktrees");
    unsafe { std::env::set_var("ZERON_WORKTREES_DIR", &worktrees_root) };

    let repo_dir = tmp_path.join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    git(&repo_dir, &["init", "-b", "main"]);
    git(&repo_dir, &["config", "user.email", "t@example.com"]);
    git(&repo_dir, &["config", "user.name", "Test"]);
    std::fs::write(repo_dir.join("README.md"), "hello\n").unwrap();
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "init"]);
    let repo_path = repo_dir.to_string_lossy().to_string();

    let cwds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { cwds: cwds.clone() }));
    let core = EngineCore::assemble(
        &tmp_path.join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    core.workspace
        .create_space(
            "space-worktree-run",
            &core.device_id,
            &repo_path,
            Some("Repo".into()),
            true,
        )
        .expect("create project");
    core.project_actions
        .upsert(
            "space-worktree-run",
            &repo_dir,
            None,
            ProjectActionDraft {
                name: "Setup".into(),
                command: "printf setup > setup-marker".into(),
                icon: ProjectActionIcon::Configure,
                run_on_worktree_create: true,
            },
        )
        .expect("save setup Action");

    // Mirror the composer: createChat lands first (cwd-less; the engine
    // resolves the project folder), then the queued Run carries the spec.
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": CHAT,
                "deviceId": core.device_id,
            }),
        )
        .await
        .expect("createChat");
    // Pre-title so the auto-titler's harness request stays out of the flow.
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    let first_command = core
        .doc_host
        .queue_command(
            CHAT,
            run_payload("msg-wt-1", &repo_path, Some("space-worktree-run")),
        )
        .expect("queue run command");
    wait_for(|| complete_assistant_count(&core) == 1, "first turn").await;

    let first_cwd = cwds.lock().unwrap().first().cloned().expect("run recorded");
    assert_ne!(
        first_cwd, repo_path,
        "the run must execute in a fresh worktree, not the repo folder"
    );
    let first = PathBuf::from(&first_cwd);
    assert!(
        first.starts_with(&worktrees_root),
        "worktree lands under the worktrees root: {first_cwd}"
    );
    assert!(
        first.join(".git").is_file(),
        "a linked worktree has a .git FILE"
    );
    wait_for(|| first.join("setup-marker").is_file(), "setup Action").await;
    let setup = core
        .project_actions
        .take_setup_handoff(&first_command, CHAT)
        .expect("fresh worktree setup handoff");
    assert!(setup.setup_action.is_some());
    assert!(setup.setup_error.is_none());

    // The chat row follows: cwd repointed at the worktree, branch stamped
    // with the actual zeron/<name> (the composer only knew the base).
    let chat = core
        .workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists");
    assert_eq!(chat.cwd.as_deref(), Some(first_cwd.as_str()));
    let branch = chat.branch.expect("branch stamped");
    assert!(
        branch.starts_with("zeron/"),
        "stamped branch is the worktree's own: {branch}"
    );

    // A duplicate spec-carrying Run (client retry) REUSES the checkout.
    let second_command = core
        .doc_host
        .queue_command(
            CHAT,
            run_payload("msg-wt-2", &repo_path, Some("space-worktree-run")),
        )
        .expect("queue second run");
    wait_for(|| complete_assistant_count(&core) == 2, "second turn").await;
    let second_cwd = cwds.lock().unwrap().get(1).cloned().expect("second run");
    assert_eq!(
        second_cwd, first_cwd,
        "a second Run with the spec must reuse the chat's worktree"
    );
    let minted = std::fs::read_dir(worktrees_root.join("repo"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(minted, 1, "exactly one worktree minted for the chat");
    let reused = core
        .project_actions
        .take_setup_handoff(&second_command, CHAT)
        .expect("reuse completion handoff");
    assert!(reused.setup_action.is_none(), "setup must not run on reuse");
    assert!(reused.setup_error.is_none());

    core.shutdown().await;
}
