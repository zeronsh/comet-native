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
                    data_dir: dirs_data_dir(),
                    edge_url: std::env::var("COMET_EDGE_URL")
                        .unwrap_or_else(|_| "http://localhost:26640".into()),
                    ipc_port: 26654,
                });
                engine.run().await
            })
        }
        None => {
            // TODO(M3): connect-or-embed engine before opening the window.
            comet_ui::run_app();
            Ok(())
        }
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}
