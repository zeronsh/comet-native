//! comet — headed by default; `comet headless` runs the engine alone.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "comet", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (VPS / remote device mode).
    Headless,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = comet_engine::Engine::new(comet_engine::EngineConfig {
                    data_dir: std::env::var_os("COMET_DATA_DIR")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(dirs_data_dir),
                    edge_url: std::env::var("COMET_EDGE_URL")
                        .unwrap_or_else(|_| "http://localhost:26640".into()),
                    // TODO(M4): real auth; until then an explicit token enables sync.
                    edge_token: std::env::var("COMET_EDGE_TOKEN").ok(),
                    ipc_port: std::env::var("COMET_IPC_PORT")
                        .ok()
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(26654),
                    default_harness: comet_engine::HarnessId::ClaudeCode,
                    // TODO(M4 auth): org comes from the signed-in session; until then
                    // COMET_ORG_ID (dev default "dev-org") scopes the workspace room.
                    org_id: std::env::var("COMET_ORG_ID").ok(),
                });
                engine.run().await
            })
        }
        None => {
            // Headed: the UI probes COMET_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            comet_ui::run_app(comet_ui::UiConfig {
                data_dir: std::env::var_os("COMET_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port: std::env::var("COMET_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(26654),
                edge_url: std::env::var("COMET_EDGE_URL")
                    .unwrap_or_else(|_| "http://localhost:26640".into()),
                edge_token: std::env::var("COMET_EDGE_TOKEN").ok(),
                default_harness: comet_ui::HarnessId::ClaudeCode,
            });
            Ok(())
        }
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}
