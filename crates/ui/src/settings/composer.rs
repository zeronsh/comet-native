//! Sticky composer defaults — the new-chat "remember my last picks" store
//! (comet parity: localStorage `comet.composer.defaults:v1`, defaults.ts).
//!
//! A small JSON file beside `ui-settings.json` (that file is the shell's and
//! is saved debounced from its own boot-time copy, so the composer keeps its
//! own file rather than racing it): last harness, last model per harness
//! (id + label, so the chip names the pick before the model list loads),
//! and last reasoning level. Written synchronously on every pick (picks are
//! rare); corrupt or missing files fall back to defaults.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use comet_proto::{HarnessId, ReasoningLevel};

const FILE_NAME: &str = "composer-defaults.json";

/// Remembered model per harness — id plus display label, mirroring comet's
/// `modelByHarness` storing the full `Model` object "so the pill never flashes
/// a raw id or 'Default'".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedModel {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ComposerDefaults {
    /// Last harness picked on the new-chat canvas.
    pub harness: Option<HarnessId>,
    /// Last model picked, per harness (restored on harness switch).
    pub model_by_harness: HashMap<HarnessId, RememberedModel>,
    /// Last reasoning level picked (global, like comet's `reasoning` key).
    pub reasoning: Option<ReasoningLevel>,
}

impl ComposerDefaults {
    /// Load from `{data_dir}/composer-defaults.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<ComposerDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "composer-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// The remembered model for a harness, if any.
    pub fn model_for(&self, harness: HarnessId) -> Option<&RememberedModel> {
        self.model_by_harness.get(&harness)
    }

    /// Remember a pick (comet `saveDefaults({ harness, modelByHarness })`).
    pub fn remember_model(&mut self, harness: HarnessId, id: String, label: String) {
        self.harness = Some(harness);
        self.model_by_harness
            .insert(harness, RememberedModel { id, label });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = ComposerDefaults {
            harness: Some(HarnessId::ClaudeCode),
            reasoning: Some(ReasoningLevel::XHigh),
            ..Default::default()
        };
        defaults.remember_model(
            HarnessId::ClaudeCode,
            "claude-fable-5".into(),
            "Fable 5".into(),
        );
        defaults.remember_model(HarnessId::Codex, "gpt-5.2-codex".into(), "GPT-5.2".into());
        defaults.save(dir.path()).unwrap();
        let loaded = ComposerDefaults::load(dir.path());
        assert_eq!(loaded, defaults);
        assert_eq!(
            loaded.model_for(HarnessId::ClaudeCode).map(|m| &*m.label),
            Some("Fable 5")
        );
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
        std::fs::write(ComposerDefaults::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
    }

    #[test]
    fn remember_model_updates_harness_and_row() {
        let mut defaults = ComposerDefaults::default();
        defaults.remember_model(HarnessId::Codex, "m1".into(), "One".into());
        defaults.remember_model(HarnessId::Codex, "m2".into(), "Two".into());
        assert_eq!(defaults.harness, Some(HarnessId::Codex));
        assert_eq!(defaults.model_for(HarnessId::Codex).map(|m| &*m.id), Some("m2"));
        assert!(defaults.model_for(HarnessId::ClaudeCode).is_none());
    }
}
