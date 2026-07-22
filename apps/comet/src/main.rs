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

/// Production edge (Cloudflare Worker + Durable Objects on the zeron.sh zone).
/// `COMET_EDGE_URL` overrides (local dev / self-hosting).
const DEFAULT_EDGE_URL: &str = "https://edge.comet.zeron.sh";

/// Production WorkOS AuthKit client id — public knowledge (it appears in every
/// authorize URL), so baking it in is safe. Overridden by `COMET_WORKOS_CLIENT_ID`;
/// set it to the empty string — or set a dev bearer via `COMET_EDGE_TOKEN` — to
/// force dev-mode auth instead.
const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KWD0E9SYPV2WE4QN9TP9DP0W";

fn edge_url_from_env() -> String {
    std::env::var("COMET_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EDGE_URL.into())
}

/// WorkOS client id resolution: explicit env wins (empty string = dev mode);
/// otherwise a `COMET_EDGE_TOKEN` dev bearer keeps dev mode (smoke tests,
/// local wrangler); otherwise the baked production client id — so a bare
/// `comet headless` signs in against production with zero configuration.
fn workos_client_id_from_env(edge_token: &Option<String>) -> Option<String> {
    match std::env::var("COMET_WORKOS_CLIENT_ID") {
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(v),
        Err(_) if edge_token.is_some() => None,
        Err(_) => Some(DEFAULT_WORKOS_CLIENT_ID.into()),
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                // Dev-mode bearer (no WorkOS): an explicit token enables sync.
                let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
                let engine = comet_engine::Engine::new(comet_engine::EngineConfig {
                    data_dir: std::env::var_os("COMET_DATA_DIR")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(dirs_data_dir),
                    edge_url: edge_url_from_env(),
                    ipc_port: std::env::var("COMET_IPC_PORT")
                        .ok()
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(27654),
                    default_harness: harness_from_env(),
                    // WorkOS mode: the signed-in session's org wins; COMET_ORG_ID (dev
                    // default "dev-org") scopes the workspace room otherwise.
                    org_id: std::env::var("COMET_ORG_ID").ok(),
                    // Real auth against production by default; see
                    // `workos_client_id_from_env` for the dev-mode escape hatches.
                    workos_client_id: workos_client_id_from_env(&edge_token),
                    edge_token,
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
                    .unwrap_or(27654),
                edge_url: edge_url_from_env(),
                edge_token: std::env::var("COMET_EDGE_TOKEN").ok(),
                default_harness: comet_ui::HarnessId::ClaudeCode,
            });
            Ok(())
        }
    }
}

/// `COMET_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> comet_engine::HarnessId {
    match std::env::var("COMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => comet_engine::HarnessId::Mock,
        Ok("codex") => comet_engine::HarnessId::Codex,
        Ok("cursor") => comet_engine::HarnessId::Cursor,
        _ => comet_engine::HarnessId::ClaudeCode,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}
