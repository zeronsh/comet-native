//! HarnessRegistry — the engine's harness catalog: eager instances (mock) plus lazy
//! slots resolved on first use (claude-code spawns subprocess discovery; codex/cursor
//! later). Lazy slots carry a static descriptor so `ListHarnesses` never forces a spawn.
//!
//! Also owns the device's harness ENABLEMENT (Settings → Agents): which harnesses
//! this device's composer offers, persisted in `{data_dir}/harness-prefs.json`.
//! Per-device because CLI installs are — a viewer retargets the settings page at
//! another device and edits THAT device's set over the forwarded RPCs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use zeron_harness::{Harness, HarnessError, mock::MockHarness};
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, SteeringMode};

/// What `ListHarnesses` reports per harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// Whether the agent's CLI is present on the listing device (the settings
    /// enable-gate). Defaults true so catalogs from engines predating the
    /// field never read as uninstallable.
    #[serde(default = "default_installed")]
    pub installed: bool,
    /// Whether the listing device offers this harness (Settings → Agents).
    /// `None` — the catalog came from an engine predating the setting — means
    /// "unknown": consumers fall back to detection (see [`descriptor_enabled`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn default_installed() -> bool {
    true
}

/// Whether detection alone may switch a harness on. The mock always "resolves"
/// but is a test rig, and counting it would let the last real agent be turned
/// off (the composer needs something it can actually run); the dev rig opts it
/// in through `COMET_HARNESS=mock`, which bypasses enablement in the pickers.
fn auto_enabled(id: HarnessId) -> bool {
    id != HarnessId::Mock
}

/// A descriptor's effective enabled flag. `None` — a catalog from an engine
/// predating the setting — falls back to detection, the same rule new devices
/// start from (see [`HarnessRegistry::enabled_set`]).
pub fn descriptor_enabled(descriptor: &HarnessDescriptor) -> bool {
    descriptor
        .enabled
        .unwrap_or_else(|| descriptor.installed && auto_enabled(descriptor.id))
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        supports_steering: harness.supports_steering(),
        steering_mode: harness.steering_mode(),
        reasoning_levels: harness.reasoning_levels().to_vec(),
        installed: harness.installed(),
        enabled: None,
    }
}

/// The persisted shape of `harness-prefs.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HarnessPrefsFile {
    /// The user's explicit opt-OUTS. Enablement otherwise follows detection,
    /// so the file only records "no" — an agent installed later turns itself
    /// on without a trip to Settings.
    disabled: Vec<HarnessId>,
    /// The allow-list written back when enablement was a fixed default set.
    /// Read once, folded into `disabled`, and never written again.
    #[serde(skip_serializing)]
    enabled: Option<Vec<HarnessId>>,
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;
type InstalledProbe = Box<dyn Fn() -> bool + Send + Sync>;

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        descriptor: HarnessDescriptor,
        /// Re-run on every `descriptors()` call — a CLI installed mid-session
        /// shows up on the next settings/picker open, no restart needed.
        installed: InstalledProbe,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
    /// This device's enabled set; `None` inner value = the default set.
    prefs: Mutex<HarnessPrefsFile>,
    /// Where the prefs persist; `None` (tests, bare registries) skips writes.
    prefs_path: Mutex<Option<PathBuf>>,
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
            prefs: Mutex::new(HarnessPrefsFile::default()),
            prefs_path: Mutex::new(None),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn prefs(&self) -> MutexGuard<'_, HarnessPrefsFile> {
        self.prefs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load `harness-prefs.json` from the engine data dir and remember the
    /// path for writes. Corrupt/missing files fall back to the default set.
    pub fn load_prefs(&self, data_dir: &Path) {
        let path = data_dir.join("harness-prefs.json");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<HarnessPrefsFile>(&text).ok())
            .unwrap_or_default();
        *self.prefs() = loaded;
        *self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path);
        self.migrate_legacy_prefs();
    }

    /// Fold a legacy allow-list into the opt-out shape: a registered harness
    /// missing from it was a deliberate "no", so it stays off. Rewrites the
    /// file once, which is what lets later installs auto-enable.
    fn migrate_legacy_prefs(&self) {
        let legacy = { self.prefs().enabled.take() };
        let Some(legacy) = legacy else { return };
        let registered: Vec<HarnessId> = self.order().iter().copied().collect();
        let disabled: Vec<HarnessId> = registered
            .into_iter()
            .filter(|id| auto_enabled(*id) && !legacy.contains(id))
            .collect();
        self.prefs().disabled = disabled;
        self.persist_prefs();
    }

    /// What this device offers: every harness whose CLI is FOUND, minus the
    /// user's explicit opt-outs. Enablement follows detection, so installing
    /// an agent is all it takes for it to appear in the composer.
    pub fn enabled_set(&self) -> Vec<HarnessId> {
        // Both guards drop before the installed probes run: `descriptors()`
        // takes `slots` then `order`, so holding `order` across a probe (which
        // takes `slots`) would invert the lock order.
        let registered: Vec<HarnessId> = self.order().iter().copied().collect();
        let disabled = self.prefs().disabled.clone();
        registered
            .into_iter()
            .filter(|id| auto_enabled(*id) && !disabled.contains(id) && self.installed_for(*id))
            .collect()
    }

    /// Whether this device's CLI probe passes for `id` (no spawn, no resolve).
    fn installed_for(&self, id: HarnessId) -> bool {
        match self.slots().get(&id) {
            Some(Slot::Ready(harness)) => harness.installed(),
            Some(Slot::Lazy { installed, .. }) => installed(),
            None => false,
        }
    }

    /// Flip one harness's enablement and persist. Refuses unknown harnesses,
    /// enabling one whose CLI is missing (the settings gate, enforced where
    /// the state lives), and disabling the last enabled harness — under
    /// detection-based enablement everything enabled is runnable, so the
    /// last one standing is always worth protecting (the composer needs
    /// something to run). A harness whose CLI is missing is never enabled
    /// in the first place, so turning it off is a clean no-op.
    pub fn set_enabled(&self, id: HarnessId, on: bool) -> Result<(), String> {
        if !self.slots().contains_key(&id) {
            return Err(format!("unknown harness {id:?}"));
        }
        if on && !auto_enabled(id) {
            return Err(format!("{id:?} cannot be enabled from Settings"));
        }
        if on && !self.installed_for(id) {
            return Err(format!("{id:?} CLI is not installed on this device"));
        }
        let enabled = self.enabled_set();
        match (on, enabled.contains(&id)) {
            (true, false) => {
                self.prefs().disabled.retain(|h| *h != id);
            }
            (false, true) => {
                if enabled.len() == 1 {
                    return Err("cannot disable the last enabled harness".into());
                }
                self.prefs().disabled.push(id);
            }
            _ => return Ok(()),
        }
        self.persist_prefs();
        Ok(())
    }

    /// Best-effort atomic write (temp + rename, the ui-settings pattern).
    fn persist_prefs(&self) {
        let Some(path) = self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let json = match serde_json::to_string_pretty(&*self.prefs()) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "harness-prefs serialize failed");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
            tracing::warn!(error = %err, "harness-prefs save failed");
        }
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self.slots().insert(id, Slot::Ready(harness)).is_none() {
            self.order().push(id);
        }
    }

    /// Register a slot resolved on first `resolve` (the factory result is
    /// cached). `installed` is the CLI-presence probe run per `descriptors()`
    /// call; it must never spawn.
    pub fn register_lazy(
        &self,
        descriptor: HarnessDescriptor,
        installed: InstalledProbe,
        factory: Factory,
    ) {
        let id = descriptor.id;
        if self
            .slots()
            .insert(
                id,
                Slot::Lazy {
                    descriptor,
                    installed,
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
        let enabled = self.enabled_set();
        let slots = self.slots();
        self.order()
            .iter()
            .filter_map(|id| {
                let mut descriptor = match slots.get(id) {
                    Some(Slot::Ready(harness)) => describe(harness.as_ref()),
                    Some(Slot::Lazy {
                        descriptor,
                        installed,
                        ..
                    }) => HarnessDescriptor {
                        installed: installed(),
                        ..descriptor.clone()
                    },
                    None => return None,
                };
                descriptor.enabled = Some(enabled.contains(id));
                Some(descriptor)
            })
            .collect()
    }
}

/// The production registry: MockHarness (hidden from production pickers) plus a lazy
/// `claude-code` slot resolved through `zeron_harness` on first use (subprocess
/// discovery only happens when a run/model call actually needs it).
pub fn default_registry() -> HarnessRegistry {
    // Warm the login-shell PATH snapshot in the background so the first
    // claude/codex resolve doesn't pay the shell-startup latency inline.
    zeron_harness::shell_env::prewarm();
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
                call: zeron_proto::ToolCall::Exec {
                    command: "cargo test --workspace".into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-1".into(),
                is_error: false,
                output: None,
                diff: None,
            },
            AgentEvent::ToolCall {
                id: "mock-tool-2".into(),
                call: zeron_proto::ToolCall::Exec {
                    command: "git log -5 --oneline --decorate && git merge-base HEAD origin/main"
                        .into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-2".into(),
                is_error: false,
                output: None,
                diff: None,
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
            // Must mirror ClaudeHarness exactly — the descriptor-stability
            // rule (see the codex test below).
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::ClaudeHarness::new().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::ClaudeHarness::new()) as Arc<dyn Harness>)),
    );
    // Codex, same lazy pattern: the static descriptor mirrors AcpHarness::codex()
    // exactly (`describe()` after the first resolve must not change the
    // catalog entry) — "Codex" per the original HARNESS_LABEL, StepBoundary
    // steering via native `turn/steer`, and the unified reasoning ladder from
    // zeron_harness::codex::catalog. CLI discovery only happens when a
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
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::CodexHarness::new().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::CodexHarness::new()) as Arc<dyn Harness>)),
    );
    // Cursor via the pinned @cursor/sdk shim (NOT ACP — that surface strips
    // subagent transcripts), same lazy pattern: the static descriptor mirrors
    // CursorHarness exactly. Turn-boundary steering; no effort ladder.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Cursor,
            name: "Cursor".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::CursorHarness::new().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::CursorHarness::new()) as Arc<dyn Harness>)),
    );
    // Devin over ACP (`devin acp`), same lazy pattern: the static descriptor
    // mirrors AcpHarness::devin() exactly. No steering extension (turn
    // boundaries) and no effort ladder — Devin bakes effort into the
    // advertised model ids instead of a `thought_level` option.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Devin,
            name: "Devin".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::devin().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::devin()) as Arc<dyn Harness>)),
    );
    // Grok Build over ACP, same lazy pattern: the static descriptor mirrors
    // AcpHarness::grok() exactly. No `_session/steering` extension yet, so
    // steers deliver at turn boundaries; the effort ladder applies per
    // session via the `thought_level` config option.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Grok,
            name: "Grok".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::grok().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::grok()) as Arc<dyn Harness>)),
    );
    // Hermes Agent over ACP (`hermes acp`), same lazy pattern: the static
    // descriptor mirrors AcpHarness::hermes() exactly. No steering extension
    // (turn boundaries) and no effort ladder — Hermes exposes no effort
    // config over ACP today (hybrid reasoning is model-internal).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Hermes,
            name: "Hermes".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::hermes().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::hermes()) as Arc<dyn Harness>)),
    );
    // pi over ACP (community `pi-acp` adapter), same lazy pattern: the static
    // descriptor mirrors AcpHarness::pi() exactly — turn-boundary steering,
    // pi's thinking ladder minus its "off" tier.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Pi,
            name: "Pi".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::pi().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::pi()) as Arc<dyn Harness>)),
    );
    // opencode over its NATIVE HTTP/SSE protocol (the one the opencode
    // desktop app speaks — `opencode serve` + the /global/event bus), same
    // lazy pattern: the static descriptor mirrors OpencodeHarness exactly.
    // Turn-boundary steering; the effort ladder rides model VARIANTS (the
    // run sends the first advertised variant id for the picked level).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Opencode,
            name: "OpenCode".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::OpencodeHarness::new().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::OpencodeHarness::new()) as Arc<dyn Harness>)),
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
                installed: true,
                enabled: None,
            },
            Box::new(|| false),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::NotInstalled("nope".into()))
            }),
        );
        let listed = registry.descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Lazy Mock");
        // The listing runs the probe, not the stored placeholder.
        assert!(!listed[0].installed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "listing must not force a resolve"
        );
        assert!(registry.resolve(HarnessId::Mock).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_registry_lists_mock_claude_codex_and_grok_slots() {
        let registry = default_registry();
        let ids: Vec<HarnessId> = registry.descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                HarnessId::Mock,
                HarnessId::ClaudeCode,
                HarnessId::Codex,
                HarnessId::Cursor,
                HarnessId::Devin,
                HarnessId::Grok,
                HarnessId::Hermes,
                HarnessId::Pi,
                HarnessId::Opencode
            ]
        );
        assert!(registry.resolve(HarnessId::Mock).is_ok());
        assert!(registry.resolve(HarnessId::ClaudeCode).is_ok());
        // A codex-configured chat resolves the right harness (construction is
        // cheap; CLI discovery is deferred to models()/run()).
        let codex = registry.resolve(HarnessId::Codex).unwrap();
        assert_eq!(codex.id(), HarnessId::Codex);
        // Grok resolves through the shared ACP harness; its descriptor must
        // mirror the resolved harness (descriptor-stability rule).
        let grok = registry.resolve(HarnessId::Grok).unwrap();
        assert_eq!(grok.id(), HarnessId::Grok);
        assert_eq!(grok.display_name(), "Grok");
        assert_eq!(grok.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            grok.reasoning_levels(),
            &[
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
        // Cursor, Devin, Hermes and Pi mirror their specs the same way.
        let cursor = registry.resolve(HarnessId::Cursor).unwrap();
        assert_eq!(cursor.id(), HarnessId::Cursor);
        assert_eq!(cursor.display_name(), "Cursor");
        assert_eq!(cursor.steering_mode(), SteeringMode::TurnBoundary);
        assert!(cursor.reasoning_levels().is_empty());
        let devin = registry.resolve(HarnessId::Devin).unwrap();
        assert_eq!(devin.id(), HarnessId::Devin);
        assert_eq!(devin.display_name(), "Devin");
        assert_eq!(devin.steering_mode(), SteeringMode::TurnBoundary);
        assert!(devin.reasoning_levels().is_empty());
        let hermes = registry.resolve(HarnessId::Hermes).unwrap();
        assert_eq!(hermes.id(), HarnessId::Hermes);
        assert_eq!(hermes.display_name(), "Hermes");
        assert_eq!(hermes.steering_mode(), SteeringMode::TurnBoundary);
        assert!(hermes.reasoning_levels().is_empty());
        let opencode = registry.resolve(HarnessId::Opencode).unwrap();
        assert_eq!(opencode.id(), HarnessId::Opencode);
        assert_eq!(opencode.display_name(), "OpenCode");
        assert_eq!(opencode.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            opencode.reasoning_levels(),
            &[
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ]
        );
        let pi = registry.resolve(HarnessId::Pi).unwrap();
        assert_eq!(pi.id(), HarnessId::Pi);
        assert_eq!(pi.display_name(), "Pi");
        assert_eq!(pi.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            pi.reasoning_levels(),
            &[
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max
            ]
        );
    }

    /// Catalogs serialized by engines that predate the `installed`/`enabled`
    /// fields must keep deserializing — installed, and enabled per the
    /// detection fallback.
    #[test]
    fn descriptor_without_new_fields_parses_with_fallbacks() {
        let parse = |id: &str| -> HarnessDescriptor {
            serde_json::from_str(&format!(
                r#"{{
                    "id": "{id}",
                    "name": "x",
                    "supportsSteering": true,
                    "steeringMode": "step-boundary",
                    "reasoningLevels": []
                }}"#
            ))
            .unwrap()
        };
        let claude = parse("claude-code");
        assert!(claude.installed);
        assert_eq!(claude.enabled, None);
        // Unknown enablement follows detection: a found CLI is offered...
        assert!(descriptor_enabled(&claude));
        // ...and one this device never found is not.
        let missing = HarnessDescriptor {
            installed: false,
            ..parse("grok")
        };
        assert!(!descriptor_enabled(&missing));
    }

    /// A registry slot for the tests below: installed probe fixed, factory
    /// never expected to run.
    fn test_slot(registry: &HarnessRegistry, id: HarnessId, installed: bool) {
        registry.register_lazy(
            HarnessDescriptor {
                id,
                name: format!("{id:?}"),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
            },
            Box::new(move || installed),
            Box::new(|| Err(HarnessError::NotInstalled("test slot".into()))),
        );
    }

    /// `descriptors()` stamps the per-device enabled flag; `set_enabled`
    /// guards the gate (no enabling missing CLIs, no disabling the last one)
    /// and persists across a reload.
    #[test]
    fn enablement_stamps_guards_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        test_slot(&registry, HarnessId::ClaudeCode, true);
        test_slot(&registry, HarnessId::Codex, true);
        test_slot(&registry, HarnessId::Grok, true);
        test_slot(&registry, HarnessId::Hermes, false);

        // Enablement follows detection: the three found CLIs are on with no
        // prefs file at all, and the missing one is off.
        let flags: Vec<(HarnessId, Option<bool>)> = registry
            .descriptors()
            .into_iter()
            .map(|d| (d.id, d.enabled))
            .collect();
        assert_eq!(
            flags,
            vec![
                (HarnessId::ClaudeCode, Some(true)),
                (HarnessId::Codex, Some(true)),
                (HarnessId::Grok, Some(true)),
                (HarnessId::Hermes, Some(false)),
            ]
        );

        // The gate: a missing CLI can't be enabled; unknown ids refuse.
        assert!(registry.set_enabled(HarnessId::Hermes, true).is_err());
        assert!(registry.set_enabled(HarnessId::Pi, true).is_err());
        assert!(registry.set_enabled(HarnessId::Mock, true).is_err());
        // Installed CLIs toggle both ways; no-op flips are fine.
        registry.set_enabled(HarnessId::Grok, true).unwrap();
        registry.set_enabled(HarnessId::Grok, true).unwrap();
        registry.set_enabled(HarnessId::Codex, false).unwrap();
        registry.set_enabled(HarnessId::ClaudeCode, false).unwrap();
        // Grok is the last one standing — refusing keeps the composer usable.
        assert!(registry.set_enabled(HarnessId::Grok, false).is_err());
        assert_eq!(registry.enabled_set(), vec![HarnessId::Grok]);

        // A fresh registry over the same data dir reads the persisted opt-outs.
        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        test_slot(&reloaded, HarnessId::ClaudeCode, true);
        test_slot(&reloaded, HarnessId::Codex, true);
        test_slot(&reloaded, HarnessId::Grok, true);
        assert_eq!(reloaded.enabled_set(), vec![HarnessId::Grok]);
    }

    /// The point of following detection: an agent installed after the user has
    /// already edited Settings turns itself on, while the ones they switched
    /// off stay off. An allow-list can't express that — it can't tell "the
    /// user said no" from "this wasn't installed yet".
    #[test]
    fn newly_found_harnesses_enable_themselves_without_reviving_opt_outs() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        test_slot(&registry, HarnessId::ClaudeCode, true);
        test_slot(&registry, HarnessId::Codex, true);
        // Not installed yet — the probe flips when the user installs the CLI.
        let found = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&found);
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Grok,
                name: "Grok".into(),
                supports_steering: true,
                steering_mode: SteeringMode::TurnBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
            },
            Box::new(move || probe.load(Ordering::SeqCst)),
            Box::new(|| Err(HarnessError::NotInstalled("test slot".into()))),
        );
        registry.set_enabled(HarnessId::Codex, false).unwrap();
        assert_eq!(registry.enabled_set(), vec![HarnessId::ClaudeCode]);

        // The CLI appears mid-session; no restart, no visit to Settings.
        found.store(true, Ordering::SeqCst);
        assert_eq!(
            registry.enabled_set(),
            vec![HarnessId::ClaudeCode, HarnessId::Grok]
        );
        // The opt-out survives the reload that picks the new agent up.
        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        test_slot(&reloaded, HarnessId::ClaudeCode, true);
        test_slot(&reloaded, HarnessId::Codex, true);
        test_slot(&reloaded, HarnessId::Grok, true);
        assert_eq!(
            reloaded.enabled_set(),
            vec![HarnessId::ClaudeCode, HarnessId::Grok]
        );
    }

    /// The mock resolves on every machine, so detection alone would enable it
    /// and let the last REAL agent be switched off — leaving a composer with
    /// nothing runnable behind a guard that thinks it is covered.
    #[test]
    fn detection_never_enables_the_mock() {
        let registry = default_registry();
        let enabled = registry.enabled_set();
        assert!(!enabled.contains(&HarnessId::Mock), "{enabled:?}");
        let mock = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Mock)
            .expect("mock slot");
        assert!(mock.installed);
        assert_eq!(mock.enabled, Some(false));
    }

    /// A prefs file from the fixed-default era is an allow-list: everything
    /// registered and absent from it was a deliberate "no", so it converts to
    /// opt-outs rather than silently re-enabling on the next launch.
    #[test]
    fn legacy_allow_list_migrates_to_opt_outs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("harness-prefs.json"),
            r#"{ "enabled": ["claude-code"] }"#,
        )
        .unwrap();
        let registry = HarnessRegistry::new();
        test_slot(&registry, HarnessId::ClaudeCode, true);
        test_slot(&registry, HarnessId::Codex, true);
        test_slot(&registry, HarnessId::Grok, true);
        registry.load_prefs(dir.path());
        assert_eq!(registry.enabled_set(), vec![HarnessId::ClaudeCode]);

        // The rewritten file is the new shape, and the legacy key is gone.
        let text = std::fs::read_to_string(dir.path().join("harness-prefs.json")).unwrap();
        assert!(!text.contains("enabled"), "{text}");
        assert!(text.contains("codex") && text.contains("grok"), "{text}");
        // An agent registered after the migration is new, not a past "no".
        test_slot(&registry, HarnessId::Cursor, true);
        assert_eq!(
            registry.enabled_set(),
            vec![HarnessId::ClaudeCode, HarnessId::Cursor]
        );
    }

    /// The fresh-machine shape (#128): no CLIs installed at all. Under
    /// detection-based enablement nothing is enabled to begin with — no
    /// dimmed default toggles to dismiss — and switching an uninstalled
    /// harness "off" is a clean no-op, not an error.
    #[test]
    fn machine_without_clis_enables_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        test_slot(&registry, HarnessId::ClaudeCode, false);
        test_slot(&registry, HarnessId::Codex, false);

        assert_eq!(registry.enabled_set(), Vec::<HarnessId>::new());
        registry.set_enabled(HarnessId::Codex, false).unwrap();
        registry.set_enabled(HarnessId::ClaudeCode, false).unwrap();
        assert_eq!(registry.enabled_set(), Vec::<HarnessId>::new());

        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        assert_eq!(reloaded.enabled_set(), Vec::<HarnessId>::new());
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
