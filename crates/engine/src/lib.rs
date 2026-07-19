//! comet-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use comet_proto::HarnessId;

use comet_sync::DocsStore;

pub mod doc_host;
pub mod registry;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod workspace_host;

pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use workspace_host::{DEFAULT_ORG_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] comet_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] comet_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] comet_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct EngineConfig {
    /// Data directory (default `~/.comet-native`, dev `~/.comet-native-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs fully offline (sync disabled).
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Workspace-doc org (`ws/{orgId}` room). `None` = `$COMET_ORG_ID` or the dev default.
    pub org_id: Option<String>,
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub device_id: String,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash. Org id comes from `$COMET_ORG_ID`
    /// (dev default `dev-org`); use [`Self::assemble_with_org`] to pass one explicitly.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let org_id = std::env::var("COMET_ORG_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ORG_ID.to_string());
        Self::assemble_with_org(data_dir, registry, default_harness, edge, &org_id)
    }

    pub fn assemble_with_org(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        let store = Arc::new(DocsStore::open(data_dir)?);
        let journal = Arc::new(RunJournal::open(data_dir.join("journals"))?);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig { device_id: device_id.clone(), default_harness, edge: edge.clone() },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(),
                platform: std::env::consts::OS.to_string(),
                org_id: org_id.to_string(),
                edge,
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        Ok(Self { sessions, doc_host, workspace, registry, device_id })
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        Arc::new(EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
        ))
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// stamp our workspace `lastSeenAt`, and flush every open doc snapshot.
    pub async fn shutdown(&self) {
        self.sessions.shutdown().await;
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Run until ctrl-c. M2: sessions engine + doc host + command executor + IPC server.
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");
        let edge = config.edge_token.as_ref().map(|token| EdgeConfig {
            url: config.edge_url.clone(),
            token: token.clone(),
        });
        let core = match &config.org_id {
            Some(org_id) => EngineCore::assemble_with_org(
                &config.data_dir,
                Arc::new(default_registry()),
                config.default_harness,
                edge,
                org_id,
            )?,
            None => EngineCore::assemble(
                &config.data_dir,
                Arc::new(default_registry()),
                config.default_harness,
                edge,
            )?,
        };
        tracing::info!(device_id = %core.device_id, "engine core assembled");

        let listener =
            tokio::net::TcpListener::bind(("127.0.0.1", config.ipc_port)).await?;
        tracing::info!(port = config.ipc_port, "IPC server listening");
        let server = tokio::spawn(comet_rpc::serve_ws_listener(listener, core.rpc_service()));

        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");
        server.abort();
        core.shutdown().await;
        Ok(())
    }
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name() -> String {
    std::env::var("COMET_DEVICE_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        Ok(_) | Err(_) => {
            let id = new_id();
            std::fs::write(&path, &id)?;
            Ok(id)
        }
    }
}
