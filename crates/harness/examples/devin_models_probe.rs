//! Live account catalog and optional ACP run (requires authenticated Devin).
//!
//!     cargo run -p zeron-harness --example devin_models_probe
//!     cargo run -p zeron-harness --example devin_models_probe -- gpt-6-astra-medium

use futures::StreamExt;
use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let harness = AcpHarness::devin();
    for attempt in 1..=2 {
        let start = std::time::Instant::now();
        let models = harness.models().await?;
        println!(
            "Discovery {attempt}: {} variants in {:?}",
            models.len(),
            start.elapsed()
        );
        for model in models.iter().filter(|m| m.id.contains("astra")) {
            println!("  {} — {}", model.id, model.label);
        }
    }
    let Some(model) = std::env::args().nth(1) else {
        return Ok(());
    };
    let (_steering, steering) = tokio::sync::mpsc::channel(8);
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = RunRequest {
        prompt: "Reply with exactly: Devin model discovery verified. Do not use tools.".into(),
        harness: None,
        model: Some(model.clone()),
        reasoning: None,
        model_options: Default::default(),
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    println!("Running real Devin ACP with {model}");
    let mut stream = harness.run(request, controls).await?;
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        while let Some(event) = stream.next().await {
            match event? {
                AgentEvent::TextDelta { text } => print!("{text}"),
                AgentEvent::Done { status, error, .. } => {
                    println!("\nDone: {status:?}");
                    anyhow::ensure!(status == DoneStatus::Completed, "{error:?}");
                    return Ok(());
                }
                _ => {}
            }
        }
        anyhow::bail!("stream closed without Done")
    })
    .await?
}
