//! comet-engine — the headless backend: sessions engine, doc host + command executor,
//! repos/worktrees/diff sync, terminals, uploads, agent accounts, auth, device-room host.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3.

pub struct EngineConfig {
    /// Data directory (default `~/.comet-native`, dev `~/.comet-native-dev`).
    pub data_dir: std::path::PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
}

pub struct Engine {
    pub config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Run until shutdown. M2: sessions engine + doc host + IPC server.
    pub async fn run(self) -> anyhow::Result<()> {
        tracing::info!(data_dir = %self.config.data_dir.display(), "engine starting (scaffold)");
        Ok(())
    }
}
