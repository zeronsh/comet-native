//! Zeron's source-neutral theme domain model.
//!
//! Runtime code consumes complete [`ThemeVariant`] values. Import formats such
//! as VS Code are deliberately isolated in [`vscode`], so a component never
//! needs to understand a workbench color id or TextMate scope.

mod builtins;
mod library;
pub mod vscode;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use builtins::builtin_registry;
pub use library::{
    CustomThemeEntry, CustomThemeLibrary, CustomThemeSource, CustomThemeStatus, InstallMode,
};

fn custom_families() -> &'static RwLock<Vec<ThemeFamily>> {
    static CUSTOM: OnceLock<RwLock<Vec<ThemeFamily>>> = OnceLock::new();
    CUSTOM.get_or_init(|| RwLock::new(Vec::new()))
}

/// Replace the process-wide custom portion of the runtime registry.
///
/// The UI calls this after loading or mutating the durable theme library. The
/// returned registry remains source-neutral: renderers still see only resolved
/// families and variants.
pub fn replace_custom_families(families: Vec<ThemeFamily>) {
    *custom_families()
        .write()
        .expect("custom theme registry lock was poisoned") = families;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    pub const ALL: [Self; 2] = [Self::Light, Self::Dark];

    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfaceTreatment {
    #[default]
    Opaque,
    Frosted,
}

/// The device-local policy applied to a theme variant's recommended surface
/// treatment. This is deliberately independent from theme and accent choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfacePreference {
    #[default]
    ThemeDefault,
    Frosted,
    Opaque,
}

impl SurfacePreference {
    pub const ALL: [Self; 3] = [Self::ThemeDefault, Self::Frosted, Self::Opaque];

    pub fn resolve(self, recommended: SurfaceTreatment) -> SurfaceTreatment {
        match self {
            Self::ThemeDefault => recommended,
            Self::Frosted => SurfaceTreatment::Frosted,
            Self::Opaque => SurfaceTreatment::Opaque,
        }
    }
}

/// A color serialized as `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub fn with_alpha(self, alpha: f32) -> Self {
        Self {
            a: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
            ..self
        }
    }

    pub fn blend_over(self, background: Self) -> Self {
        let alpha = self.a as f32 / 255.0;
        let blend = |front: u8, back: u8| {
            (front as f32 * alpha + back as f32 * (1.0 - alpha)).round() as u8
        };
        Self::rgb(
            blend(self.r, background.r),
            blend(self.g, background.g),
            blend(self.b, background.b),
        )
    }

    pub fn mix(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
        Self::rgba(
            mix(self.r, other.r),
            mix(self.g, other.g),
            mix(self.b, other.b),
            mix(self.a, other.a),
        )
    }

    pub fn contrast(self, background: Self) -> f32 {
        let foreground = if self.a == 255 {
            self
        } else {
            self.blend_over(background)
        };
        let (a, b) = (foreground.luminance(), background.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    pub fn best_on_color(self) -> Self {
        if Self::WHITE.contrast(self) >= Self::BLACK.contrast(self) {
            Self::WHITE
        } else {
            Self::BLACK
        }
    }

    /// Move toward black or white until the requested contrast is met.
    pub fn ensure_contrast(self, background: Self, minimum: f32) -> Self {
        if self.contrast(background) >= minimum {
            return self;
        }
        let target = if Self::BLACK.contrast(background) >= Self::WHITE.contrast(background) {
            Self::BLACK
        } else {
            Self::WHITE
        };
        for step in 1..=20 {
            let candidate = self.mix(target, step as f32 / 20.0);
            if candidate.contrast(background) >= minimum {
                return candidate;
            }
        }
        target
    }

    fn luminance(self) -> f32 {
        let linear = |channel: u8| {
            let value = channel as f32 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }
}

impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.r, self.g, self.b, self.a
            )
        }
    }
}

impl FromStr for Color {
    type Err = ColorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim().strip_prefix('#').ok_or(ColorParseError)?;
        let expand = |c: u8| (c << 4) | c;
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(ColorParseError),
        };
        let pair = |bytes: &[u8]| -> Result<u8, ColorParseError> {
            Ok((nibble(bytes[0])? << 4) | nibble(bytes[1])?)
        };
        let bytes = value.as_bytes();
        match bytes.len() {
            3 => Ok(Self::rgb(
                expand(nibble(bytes[0])?),
                expand(nibble(bytes[1])?),
                expand(nibble(bytes[2])?),
            )),
            4 => Ok(Self::rgba(
                expand(nibble(bytes[0])?),
                expand(nibble(bytes[1])?),
                expand(nibble(bytes[2])?),
                expand(nibble(bytes[3])?),
            )),
            6 => Ok(Self::rgb(
                pair(&bytes[0..2])?,
                pair(&bytes[2..4])?,
                pair(&bytes[4..6])?,
            )),
            8 => Ok(Self::rgba(
                pair(&bytes[0..2])?,
                pair(&bytes[2..4])?,
                pair(&bytes[4..6])?,
                pair(&bytes[6..8])?,
            )),
            _ => Err(ColorParseError),
        }
    }
}

impl Serialize for Color {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("expected a CSS hex color (#rgb, #rgba, #rrggbb, or #rrggbbaa)")]
pub struct ColorParseError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccentPreset {
    #[default]
    Zeron,
    Orange,
    Amber,
    Green,
    Cyan,
    Blue,
    Pink,
}

impl AccentPreset {
    pub const ALL: [Self; 7] = [
        Self::Zeron,
        Self::Orange,
        Self::Amber,
        Self::Green,
        Self::Cyan,
        Self::Blue,
        Self::Pink,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Zeron => "Zeron",
            Self::Orange => "Orange",
            Self::Amber => "Amber",
            Self::Green => "Green",
            Self::Cyan => "Cyan",
            Self::Blue => "Blue",
            Self::Pink => "Pink",
        }
    }

    pub fn color(self, appearance: Appearance) -> Color {
        let (dark, light) = match self {
            Self::Zeron => ("#8b7cf6", "#5b43e8"),
            Self::Orange => ("#fb923c", "#c2410c"),
            Self::Amber => ("#fbbf24", "#a16207"),
            Self::Green => ("#4ade80", "#15803d"),
            Self::Cyan => ("#22d3ee", "#0e7490"),
            Self::Blue => ("#60a5fa", "#2563eb"),
            Self::Pink => ("#f472b6", "#be185d"),
        };
        if appearance.is_dark() { dark } else { light }
            .parse()
            .expect("built-in accent colors are valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccentSelection {
    #[default]
    ThemeDefault,
    Preset(AccentPreset),
}

impl AccentSelection {
    pub fn label(self) -> &'static str {
        match self {
            Self::ThemeDefault => "Theme default",
            Self::Preset(preset) => preset.label(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccentRoles {
    pub primary: Color,
    pub strong: Color,
    pub wash: Color,
    pub on: Color,
    pub selection: Color,
    pub caret: Color,
    pub activity: Color,
    pub glyph: [Color; 3],
}

impl AccentRoles {
    pub fn derive(primary: Color, appearance: Appearance, background: Color) -> Self {
        let primary = primary.ensure_contrast(background, 3.0);
        let on = primary.best_on_color();
        let mut strong = primary;
        if on.contrast(strong) < 4.5 {
            strong = strong.ensure_contrast(on, 4.5);
        }
        let light = primary.mix(
            if appearance.is_dark() {
                Color::WHITE
            } else {
                background
            },
            if appearance.is_dark() { 0.28 } else { 0.18 },
        );
        let deep = primary.mix(
            if appearance.is_dark() {
                Color::BLACK
            } else {
                Color::BLACK
            },
            if appearance.is_dark() { 0.18 } else { 0.26 },
        );
        Self {
            primary,
            strong,
            wash: primary.with_alpha(if appearance.is_dark() { 0.22 } else { 0.12 }),
            on,
            selection: primary.with_alpha(if appearance.is_dark() { 0.35 } else { 0.24 }),
            caret: primary,
            activity: primary,
            glyph: [light, primary, deep],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeSelection {
    pub light: String,
    pub dark: String,
}

impl Default for ThemeSelection {
    fn default() -> Self {
        Self {
            light: "zeron-light".into(),
            dark: "zeron-dark".into(),
        }
    }
}

impl ThemeSelection {
    pub fn variant_id(&self, appearance: Appearance) -> &str {
        if appearance.is_dark() {
            &self.dark
        } else {
            &self.light
        }
    }

    pub fn set_variant(&mut self, appearance: Appearance, id: impl Into<String>) {
        if appearance.is_dark() {
            self.dark = id.into();
        } else {
            self.light = id.into();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeSource {
    pub format: String,
    pub url: String,
    pub revision: String,
    pub license: String,
    pub asset_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeColors {
    pub background: Color,
    pub shell: Color,
    pub raised: Color,
    pub card: Color,
    pub dialog: Color,
    pub overlay: Color,
    pub hover: Color,
    pub active: Color,
    pub border: Color,
    pub border_strong: Color,
    pub text: Color,
    pub text_muted: Color,
    pub text_faint: Color,
    pub solid: Color,
    pub on_solid: Color,
    pub danger: Color,
    pub danger_muted: Color,
    pub warning: Color,
    pub warning_muted: Color,
    pub success: Color,
    pub success_muted: Color,
    pub input: Color,
    pub cursor: Color,
    pub diff_add: Color,
    pub diff_delete: Color,
    pub diff_hunk: Color,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalPalette {
    pub background: Color,
    pub foreground: Color,
    pub selection: Color,
    pub ansi: [Color; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeVariant {
    pub id: String,
    pub family_id: String,
    pub name: String,
    pub appearance: Appearance,
    /// The author's recommended treatment. Runtime applies the user's
    /// [`SurfacePreference`] before components consume the variant.
    #[serde(alias = "surfaceTreatment")]
    pub recommended_surface_treatment: SurfaceTreatment,
    pub colors: ThemeColors,
    pub accent: AccentRoles,
    pub syntax: BTreeMap<String, Color>,
    pub terminal: TerminalPalette,
    pub source: ThemeSource,
}

impl ThemeVariant {
    pub fn accent_for(&self, selection: AccentSelection) -> AccentRoles {
        match selection {
            AccentSelection::ThemeDefault => self.accent,
            AccentSelection::Preset(preset) => AccentRoles::derive(
                preset.color(self.appearance),
                self.appearance,
                self.colors.background,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeFamily {
    pub id: String,
    pub name: String,
    pub variants: Vec<ThemeVariant>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeRegistry {
    pub families: Vec<ThemeFamily>,
}

impl ThemeRegistry {
    pub fn builtin() -> &'static Self {
        builtin_registry()
    }

    /// Built-ins plus the currently installed custom families.
    pub fn active() -> Self {
        let mut families = Self::builtin().families.clone();
        families.extend(
            custom_families()
                .read()
                .expect("custom theme registry lock was poisoned")
                .iter()
                .cloned(),
        );
        Self { families }
    }

    pub fn variant(&self, id: &str) -> Option<&ThemeVariant> {
        self.families
            .iter()
            .flat_map(|family| &family.variants)
            .find(|variant| variant.id == id)
    }

    pub fn variants_for(&self, appearance: Appearance) -> impl Iterator<Item = &ThemeVariant> {
        self.families
            .iter()
            .flat_map(|family| &family.variants)
            .filter(move |variant| variant.appearance == appearance)
    }

    pub fn resolve(&self, selection: &ThemeSelection, appearance: Appearance) -> &ThemeVariant {
        self.variant(selection.variant_id(appearance))
            .or_else(|| {
                self.variant(if appearance.is_dark() {
                    "zeron-dark"
                } else {
                    "zeron-light"
                })
            })
            .expect("the built-in registry always contains both Zeron variants")
    }

    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        let mut ids = HashSet::new();
        for family in &self.families {
            for variant in &family.variants {
                if variant.id.trim().is_empty() || !ids.insert(&variant.id) {
                    issues.push(ValidationIssue::structural_error(
                        &variant.id,
                        "variant id must be unique",
                    ));
                }
                validate_contrast(
                    &mut issues,
                    variant,
                    "text",
                    variant.colors.text,
                    variant.colors.background,
                    4.5,
                );
                validate_contrast(
                    &mut issues,
                    variant,
                    "muted text",
                    variant.colors.text_muted,
                    variant.colors.background,
                    4.5,
                );
                validate_contrast(
                    &mut issues,
                    variant,
                    "accent",
                    variant.accent.primary,
                    variant.colors.background,
                    3.0,
                );
                validate_contrast(
                    &mut issues,
                    variant,
                    "on-accent",
                    variant.accent.on,
                    variant.accent.strong,
                    4.5,
                );
                validate_contrast(
                    &mut issues,
                    variant,
                    "terminal foreground",
                    variant.terminal.foreground,
                    variant.terminal.background,
                    4.5,
                );
                if variant.source.url.is_empty()
                    || variant.source.revision.is_empty()
                    || variant.source.license.is_empty()
                    || variant.source.asset_hash.is_empty()
                {
                    issues.push(ValidationIssue::structural_error(
                        &variant.id,
                        "source provenance is incomplete",
                    ));
                }
                for (index, color) in variant.terminal.ansi.iter().enumerate() {
                    // Slots 0 and 8 are structural black/dim colors. Every
                    // chromatic normal and bright slot is expected to remain
                    // distinguishable as terminal text.
                    if index % 8 == 0 {
                        continue;
                    }
                    if color.contrast(variant.terminal.background) < 3.0 {
                        issues.push(ValidationIssue::contrast_warning(
                            &variant.id,
                            format!("terminal ANSI slot {index} is below 3:1"),
                        ));
                    }
                }
            }
        }
        issues
    }
}

fn validate_contrast(
    issues: &mut Vec<ValidationIssue>,
    variant: &ThemeVariant,
    role: &str,
    foreground: Color,
    background: Color,
    minimum: f32,
) {
    let actual = foreground.contrast(background);
    if actual < minimum {
        issues.push(ValidationIssue::contrast_error(
            &variant.id,
            format!("{role} contrast is {actual:.2}:1; expected {minimum:.1}:1"),
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ValidationSeverity {
    Warning,
    Error,
}

/// Whether a validation issue makes a theme structurally unsafe to install or
/// describes a visual quality problem that the compiler/runtime can harden.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ValidationCategory {
    Structural,
    #[default]
    Contrast,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub variant_id: String,
    #[serde(default)]
    pub category: ValidationCategory,
    pub severity: ValidationSeverity,
    pub message: String,
}

impl ValidationIssue {
    fn structural_error(variant_id: &str, message: impl Into<String>) -> Self {
        Self {
            variant_id: variant_id.into(),
            category: ValidationCategory::Structural,
            severity: ValidationSeverity::Error,
            message: message.into(),
        }
    }

    fn contrast_error(variant_id: &str, message: impl Into<String>) -> Self {
        Self {
            variant_id: variant_id.into(),
            category: ValidationCategory::Contrast,
            severity: ValidationSeverity::Error,
            message: message.into(),
        }
    }

    fn contrast_warning(variant_id: &str, message: impl Into<String>) -> Self {
        Self {
            variant_id: variant_id.into(),
            category: ValidationCategory::Contrast,
            severity: ValidationSeverity::Warning,
            message: message.into(),
        }
    }

    pub fn is_blocking(&self) -> bool {
        self.category == ValidationCategory::Structural
            && self.severity == ValidationSeverity::Error
    }
}

/// Stable visual QA scenes every bundled variant must be reviewed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VisualFixture {
    Sidebar,
    TranscriptMarkdown,
    TranscriptCode,
    Composer,
    PickerPopover,
    AppearanceSettings,
    Diff,
    Terminal,
    EmptyState,
    Dialog,
}

impl VisualFixture {
    pub const ALL: [Self; 10] = [
        Self::Sidebar,
        Self::TranscriptMarkdown,
        Self::TranscriptCode,
        Self::Composer,
        Self::PickerPopover,
        Self::AppearanceSettings,
        Self::Diff,
        Self::Terminal,
        Self::EmptyState,
        Self::Dialog,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_round_trip_all_supported_css_hex_lengths() {
        for source in ["#abc", "#abcd", "#102030", "#10203040"] {
            let color: Color = source.parse().unwrap();
            let json = serde_json::to_string(&color).unwrap();
            assert_eq!(serde_json::from_str::<Color>(&json).unwrap(), color);
        }
    }

    #[test]
    fn accent_overlay_meets_interaction_contrast() {
        for appearance in Appearance::ALL {
            let background = if appearance.is_dark() {
                Color::rgb(6, 6, 6)
            } else {
                Color::WHITE
            };
            for preset in AccentPreset::ALL {
                let roles = AccentRoles::derive(preset.color(appearance), appearance, background);
                assert!(
                    roles.primary.contrast(background) >= 3.0,
                    "{appearance:?} {preset:?}"
                );
                assert!(
                    roles.on.contrast(roles.strong) >= 4.5,
                    "{appearance:?} {preset:?}"
                );
            }
        }
    }

    #[test]
    fn surface_preference_resolves_independently_from_theme_recommendation() {
        assert_eq!(
            SurfacePreference::ThemeDefault.resolve(SurfaceTreatment::Frosted),
            SurfaceTreatment::Frosted
        );
        assert_eq!(
            SurfacePreference::ThemeDefault.resolve(SurfaceTreatment::Opaque),
            SurfaceTreatment::Opaque
        );
        for recommended in [SurfaceTreatment::Frosted, SurfaceTreatment::Opaque] {
            assert_eq!(
                SurfacePreference::Frosted.resolve(recommended),
                SurfaceTreatment::Frosted
            );
            assert_eq!(
                SurfacePreference::Opaque.resolve(recommended),
                SurfaceTreatment::Opaque
            );
        }
    }

    #[test]
    fn builtins_have_complete_provenance_and_no_validation_errors() {
        let registry = ThemeRegistry::builtin();
        assert_eq!(registry.families.len(), 19);
        assert!(registry.variant("zeron-light").is_some());
        assert!(registry.variant("zeron-dark").is_some());
        let errors: Vec<_> = registry
            .validate()
            .into_iter()
            .filter(|issue| issue.severity == ValidationSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn visual_fixture_matrix_covers_every_variant_and_scene() {
        let registry = ThemeRegistry::builtin();
        let variants = registry
            .families
            .iter()
            .map(|family| family.variants.len())
            .sum::<usize>();
        assert_eq!(variants, 30);
        assert_eq!(variants * VisualFixture::ALL.len(), 300);
    }
}
