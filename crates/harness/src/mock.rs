//! Mock harness for engine/UI tests: replays a scripted event sequence.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_proto::{AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode};

use crate::{Harness, HarnessError, RunControls};

pub struct MockHarness {
    pub script: Vec<AgentEvent>,
}

#[async_trait]
impl Harness for MockHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Mock"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![
            Model {
                id: "mock-1".into(),
                label: "Mock 1".into(),
                reasoning_levels: vec![ReasoningLevel::Medium],
                options: vec![],
            },
            // Claude-mirroring demo model: lets scripted runs carry the same
            // chip labels ("Fable 5 · High") as a real Claude session.
            Model {
                id: "mock-fable-5".into(),
                label: "Fable 5".into(),
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ],
                options: vec![],
            },
        ])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let events: Vec<Result<AgentEvent, HarnessError>> =
            self.script.iter().cloned().map(Ok).collect();
        // Optional pacing knob for demos/manual testing: `COMET_MOCK_DELAY_MS`
        // spaces the scripted events out so live-run UI states (working
        // indicator, streaming fade, trailing tool-group auto-open) are
        // observable. Unset (the default, and in tests) streams instantly.
        let delay_ms = std::env::var("COMET_MOCK_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if delay_ms == 0 {
            return Ok(futures::stream::iter(events).boxed());
        }
        let delay = std::time::Duration::from_millis(delay_ms);
        Ok(futures::stream::iter(events)
            .then(move |event| async move {
                tokio::time::sleep(delay).await;
                event
            })
            .boxed())
    }
}
