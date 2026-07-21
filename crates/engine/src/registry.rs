//! HarnessRegistry — the engine's harness catalog: eager instances (mock) plus lazy
//! slots resolved on first use (claude-code spawns subprocess discovery; codex/cursor
//! later). Lazy slots carry a static descriptor so `ListHarnesses` never forces a spawn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use comet_harness::{Harness, HarnessError, mock::MockHarness};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, SteeringMode};

/// What `ListHarnesses` reports per harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        supports_steering: harness.supports_steering(),
        steering_mode: harness.steering_mode(),
        reasoning_levels: harness.reasoning_levels().to_vec(),
    }
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        descriptor: HarnessDescriptor,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self.slots().insert(id, Slot::Ready(harness)).is_none() {
            self.order().push(id);
        }
    }

    /// Register a slot resolved on first `resolve` (the factory result is cached).
    pub fn register_lazy(&self, descriptor: HarnessDescriptor, factory: Factory) {
        let id = descriptor.id;
        if self
            .slots()
            .insert(
                id,
                Slot::Lazy {
                    descriptor,
                    factory,
                },
            )
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn resolve(&self, id: HarnessId) -> Result<Arc<dyn Harness>, HarnessError> {
        let mut slots = self.slots();
        match slots.get(&id) {
            Some(Slot::Ready(harness)) => Ok(harness.clone()),
            Some(Slot::Lazy { factory, .. }) => {
                let harness = factory()?;
                slots.insert(id, Slot::Ready(harness.clone()));
                Ok(harness)
            }
            None => Err(HarnessError::NotInstalled(format!("{id:?}"))),
        }
    }

    /// Catalog for `ListHarnesses` — never forces a lazy resolve.
    pub fn descriptors(&self) -> Vec<HarnessDescriptor> {
        let slots = self.slots();
        self.order()
            .iter()
            .filter_map(|id| match slots.get(id) {
                Some(Slot::Ready(harness)) => Some(describe(harness.as_ref())),
                Some(Slot::Lazy { descriptor, .. }) => Some(descriptor.clone()),
                None => None,
            })
            .collect()
    }
}

/// The production registry: MockHarness (hidden from production pickers) plus a lazy
/// `claude-code` slot resolved through `comet_harness` on first use (subprocess
/// discovery only happens when a run/model call actually needs it).
pub fn default_registry() -> HarnessRegistry {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: vec![
            AgentEvent::TextDelta {
                text: "## Streaming pipeline\n\nEvery turn flows through the same path:\n\n".into(),
            },
            AgentEvent::TextDelta {
                text: "1. **Doc command** — the composer queues a durable `run` entry\n2. **Host executor** — the chat's host device marks it processed, then dispatches\n3. **Fold** — events fold into parts and diff into the Loro doc every 120ms\n\n".into(),
            },
            AgentEvent::ToolCall {
                id: "mock-tool-1".into(),
                call: comet_proto::ToolCall::Exec {
                    command: "cargo test --workspace".into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-1".into(),
                is_error: false,
            },
            AgentEvent::ToolCall {
                id: "mock-tool-2".into(),
                call: comet_proto::ToolCall::Exec {
                    command: "git log -5 --oneline --decorate && git merge-base HEAD origin/main"
                        .into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-2".into(),
                is_error: false,
            },
            AgentEvent::TextDelta {
                text: "The `SegmentWriter` appends into `LoroText` so the oplog stays RLE-merged:\n\n```rust\nfolded = fold_event_into_parts(&folded, &event);\nwriter.sync(&folded)?; // 120ms coalesced commits\n```\n\nSynced to every device through the session room. *Mock harness reporting in.*".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    }));
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::ClaudeCode,
            name: "Claude Code".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            // Must mirror ClaudeHarness::reasoning_levels() exactly — the
            // descriptor-stability rule (see the codex test below).
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
        },
        Box::new(|| Ok(Arc::new(comet_harness::ClaudeHarness::new()) as Arc<dyn Harness>)),
    );
    // Codex, same lazy pattern: the static descriptor mirrors CodexHarness
    // exactly (`describe()` after the first resolve must not change the
    // catalog entry) — "Codex" per the original HARNESS_LABEL, StepBoundary
    // steering via native `turn/steer`, and the unified reasoning ladder from
    // comet_harness::codex::catalog. CLI discovery only happens when a
    // run/model call actually resolves the slot.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Codex,
            name: "Codex".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
                ReasoningLevel::Ultra,
            ],
        },
        Box::new(|| Ok(Arc::new(comet_harness::CodexHarness::new()) as Arc<dyn Harness>)),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_slot_lists_without_resolving() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = HarnessRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Mock,
                name: "Lazy Mock".into(),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
            },
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::NotInstalled("nope".into()))
            }),
        );
        let listed = registry.descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Lazy Mock");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "listing must not force a resolve"
        );
        assert!(registry.resolve(HarnessId::Mock).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_registry_lists_mock_claude_and_codex_slots() {
        let registry = default_registry();
        let ids: Vec<HarnessId> = registry.descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![HarnessId::Mock, HarnessId::ClaudeCode, HarnessId::Codex]
        );
        assert!(registry.resolve(HarnessId::Mock).is_ok());
        assert!(registry.resolve(HarnessId::ClaudeCode).is_ok());
        // A codex-configured chat resolves the right harness (construction is
        // cheap; CLI discovery is deferred to models()/run()).
        let codex = registry.resolve(HarnessId::Codex).unwrap();
        assert_eq!(codex.id(), HarnessId::Codex);
    }

    /// The Codex lazy descriptor must be indistinguishable from `describe()`
    /// after the first resolve — otherwise the catalog entry silently changes
    /// the moment the harness is used (name/ladder flip in the picker rail).
    /// (KNOWN GAP, predates this slot: the claude-code descriptor advertises
    /// `[Ultrathink]` while the resolved adapter reports `[Low..Max]` — left
    /// as-is here; flagged for its own pass.)
    #[test]
    fn codex_lazy_descriptor_matches_resolved_harness() {
        let registry = default_registry();
        let before = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        registry.resolve(HarnessId::Codex).unwrap();
        let after = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        assert_eq!(before.name, after.name);
        assert_eq!(before.supports_steering, after.supports_steering);
        assert_eq!(before.steering_mode, after.steering_mode);
        assert_eq!(before.reasoning_levels, after.reasoning_levels);
    }
}
