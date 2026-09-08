//! Live probe: opencode model discovery through the native driver — boots
//! `opencode serve`, reads `GET /provider`, prints the picker catalog
//! (connected providers only). Needs `opencode` on PATH (or
//! OPENCODE_EXECUTABLE); no provider auth required (the anonymous Zen tier
//! always connects).
//!
//!     cargo run -p zeron-harness --example opencode_models_probe

use zeron_harness::{Harness, OpencodeHarness};

#[tokio::main]
async fn main() {
    let start = std::time::Instant::now();
    match OpencodeHarness::new().models().await {
        Ok(models) => {
            for m in &models {
                eprintln!(
                    "{:40} {:28} {:?} {:?}",
                    m.id,
                    m.label,
                    m.description.as_deref().unwrap_or(""),
                    m.reasoning_levels
                );
            }
            eprintln!("--- {} models in {:?}", models.len(), start.elapsed());
        }
        Err(e) => {
            eprintln!("discovery failed: {e}");
            std::process::exit(1);
        }
    }
}
