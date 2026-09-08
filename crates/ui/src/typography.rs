//! Device-local interface typography: bundled font registration, the persisted
//! catalog choice, and the effective family installed into [`crate::theme::Theme`].

use std::{borrow::Cow, collections::BTreeSet};

use gpui::{App, Global, Rems, SharedString, Window, px, rems};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::settings::{self, SavePolicy};

/// A bundled, virtual, or device-local interface font choice.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum UiFontFamily {
    #[default]
    Geist,
    GeistMono,
    System,
    Installed(String),
}

impl UiFontFamily {
    pub fn label(&self) -> &str {
        match self {
            Self::Geist => "Geist",
            Self::GeistMono => "Geist Mono",
            Self::System => "System UI",
            Self::Installed(name) => name,
        }
    }

    pub fn family_name(&self) -> &str {
        match self {
            Self::Geist => "Geist",
            Self::GeistMono => "Geist Mono",
            Self::System => ".SystemUIFont",
            Self::Installed(name) => name,
        }
    }
}

impl Serialize for UiFontFamily {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Geist => serializer.serialize_str("geist"),
            Self::GeistMono => serializer.serialize_str("geistMono"),
            Self::System => serializer.serialize_str("system"),
            Self::Installed(name) => serializer.serialize_str(&format!("installed:{name}")),
        }
    }
}

impl<'de> Deserialize<'de> for UiFontFamily {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "geist" => Self::Geist,
            "geistMono" => Self::GeistMono,
            "system" => Self::System,
            // Preserve selections written by the previous fixed catalog. They
            // now resolve only when the family is installed on this device.
            "inter" => Self::Installed("Inter".into()),
            "atkinsonHyperlegibleNext" => Self::Installed("Atkinson Hyperlegible Next".into()),
            value if value.starts_with("installed:") && value.len() > "installed:".len() => {
                Self::Installed(value["installed:".len()..].to_owned())
            }
            _ => Self::Geist,
        })
    }
}

/// Base size used by rem-based interface text. Persisted as a number so future
/// builds can add choices without changing the settings format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UiFontSize(u8);

impl UiFontSize {
    pub const ALL: [Self; 7] = [
        Self(12),
        Self(13),
        Self(14),
        Self(15),
        Self(16),
        Self(18),
        Self(20),
    ];

    pub const fn pixels(self) -> f32 {
        self.0 as f32
    }

    pub fn label(self) -> SharedString {
        format!("{} px", self.0).into()
    }

    pub fn normalized(self) -> Self {
        Self::ALL
            .into_iter()
            .min_by_key(|candidate| candidate.0.abs_diff(self.0))
            .unwrap_or_default()
    }
}

impl Default for UiFontSize {
    fn default() -> Self {
        Self(16)
    }
}

/// Convert a size designed at the 16px baseline into a scalable interface rem.
/// Code, diffs, and terminal text deliberately keep absolute pixel sizes.
pub const fn ui_rems(pixels_at_default: f32) -> Rems {
    rems(pixels_at_default / 16.0)
}

/// Which catalog families successfully registered during this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontAvailability {
    geist: bool,
    geist_mono: bool,
    choices: Vec<UiFontFamily>,
}

impl FontAvailability {
    #[cfg(test)]
    pub fn all() -> Self {
        Self {
            geist: true,
            geist_mono: true,
            choices: vec![
                UiFontFamily::Geist,
                UiFontFamily::GeistMono,
                UiFontFamily::System,
                UiFontFamily::Installed("Arial".into()),
                UiFontFamily::Installed("Menlo".into()),
            ],
        }
    }

    pub fn choices(&self) -> &[UiFontFamily] {
        &self.choices
    }

    pub fn is_available(&self, family: &UiFontFamily) -> bool {
        match family {
            UiFontFamily::Geist => self.geist,
            UiFontFamily::GeistMono => self.geist_mono,
            UiFontFamily::System => true,
            UiFontFamily::Installed(_) => self.choices.contains(family),
        }
    }

    fn fallback(&self) -> UiFontFamily {
        if self.geist {
            UiFontFamily::Geist
        } else {
            UiFontFamily::System
        }
    }

    #[cfg(test)]
    pub(crate) fn without(mut self, family: &UiFontFamily) -> Self {
        match family {
            UiFontFamily::Geist => self.geist = false,
            UiFontFamily::GeistMono => self.geist_mono = false,
            UiFontFamily::System => {}
            UiFontFamily::Installed(_) => self.choices.retain(|choice| choice != family),
        }
        self
    }
}

impl Default for FontAvailability {
    fn default() -> Self {
        Self {
            geist: false,
            geist_mono: false,
            choices: vec![UiFontFamily::System],
        }
    }
}

/// Requested and effective typography for the process.
pub struct TypographyState {
    pub requested: UiFontFamily,
    pub effective: UiFontFamily,
    pub size: UiFontSize,
    pub availability: FontAvailability,
    /// Monotonic signal for layout caches whose measurements depend on UI
    /// typography. Kept separate from the theme style generation so palette
    /// changes do not force expensive list remeasurement.
    generation: u32,
}

impl Global for TypographyState {}

const GEIST: [&[u8]; 8] = [
    include_bytes!("../assets/fonts/Geist.ttf"),
    include_bytes!("../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-MediumItalic.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBoldItalic.ttf"),
    include_bytes!("../assets/fonts/Geist-Bold.ttf"),
    include_bytes!("../assets/fonts/Geist-BoldItalic.ttf"),
];

const GEIST_MONO: [&[u8]; 8] = [
    include_bytes!("../assets/fonts/GeistMono.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Italic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-MediumItalic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBoldItalic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Bold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-BoldItalic.ttf"),
];

fn register_family(cx: &App, family: &UiFontFamily, faces: &'static [&'static [u8]]) -> bool {
    let fonts = faces.iter().map(|face| Cow::Borrowed(*face)).collect();
    match cx.text_system().add_fonts(fonts) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(font_family = family.label(), error = %err, "failed to register bundled font family");
            false
        }
    }
}

/// Families that contain the Latin glyph GPUI relies on for text metrics.
///
/// `all_font_names` intentionally reports every system family, including
/// script-specific and symbol fonts. GPUI must reject a face without `m`, but
/// doing that only after it has been selected both falls back silently and
/// emits a warning. Inspect the installed files first so those families never
/// appear in an interface-font picker.
fn installed_families_with_latin_metrics() -> BTreeSet<String> {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();

    database
        .faces()
        .filter(|face| {
            database
                .with_face_data(face.id, |data, index| {
                    ttf_parser::Face::parse(data, index)
                        .is_ok_and(|font| font.glyph_index('m').is_some())
                })
                .unwrap_or(false)
        })
        .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
        .collect()
}

/// Register each family independently so one bad optional asset cannot hide
/// the rest of the catalog.
pub fn register_fonts(cx: &App) -> FontAvailability {
    // Capture device fonts before adding ours so the Installed section only
    // contains families supplied by the OS/user, not our embedded assets.
    let families_with_latin_metrics = installed_families_with_latin_metrics();
    let system_names: BTreeSet<_> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .filter(|name| name != "Geist" && name != "Geist Mono")
        .filter(|name| families_with_latin_metrics.contains(name))
        .collect();
    let geist = register_family(cx, &UiFontFamily::Geist, &GEIST);
    let geist_mono = register_family(cx, &UiFontFamily::GeistMono, &GEIST_MONO);
    let mut choices = vec![
        UiFontFamily::Geist,
        UiFontFamily::GeistMono,
        UiFontFamily::System,
    ];
    choices.extend(system_names.into_iter().map(UiFontFamily::Installed));
    FontAvailability {
        geist,
        geist_mono,
        choices,
    }
}

fn resolve_effective(requested: &UiFontFamily, availability: &FontAvailability) -> UiFontFamily {
    if availability.is_available(requested) {
        requested.clone()
    } else {
        availability.fallback()
    }
}

/// Install typography state before appearance builds the first [`crate::theme::Theme`].
pub fn init(
    requested: UiFontFamily,
    size: UiFontSize,
    availability: FontAvailability,
    cx: &mut App,
) {
    let effective = resolve_effective(&requested, &availability);
    cx.set_global(TypographyState {
        requested,
        effective,
        size: size.normalized(),
        availability,
        generation: 0,
    });
}

pub fn requested(cx: &App) -> UiFontFamily {
    cx.try_global::<TypographyState>()
        .map(|state| state.requested.clone())
        .unwrap_or_default()
}

pub fn effective(cx: &App) -> UiFontFamily {
    cx.try_global::<TypographyState>()
        .map(|state| state.effective.clone())
        .unwrap_or_default()
}

pub fn effective_family_name(cx: &App) -> SharedString {
    effective(cx).family_name().into()
}

pub fn font_size(cx: &App) -> UiFontSize {
    cx.try_global::<TypographyState>()
        .map(|state| state.size)
        .unwrap_or_default()
}

pub fn availability(cx: &App) -> FontAvailability {
    cx.try_global::<TypographyState>()
        .map(|state| state.availability.clone())
        .unwrap_or_default()
}

/// Monotonic id of the current effective UI typography (family + size).
/// Long-lived layout caches compare this value to invalidate measurements
/// that can change when prose wraps differently.
pub fn generation(cx: &App) -> u32 {
    cx.try_global::<TypographyState>()
        .map(|state| state.generation)
        .unwrap_or_default()
}

pub fn is_available(family: &UiFontFamily, cx: &App) -> bool {
    availability(cx).is_available(family)
}

/// Validate, apply, repaint, and persist one confirmed choice. Returns whether
/// the effective family changed (re-selecting the current family is a no-op).
pub fn set_family(family: UiFontFamily, cx: &mut App) -> bool {
    let Some(state) = cx.try_global::<TypographyState>() else {
        return false;
    };
    if !state.availability.is_available(&family) {
        return false;
    }
    let effective = resolve_effective(&family, &state.availability);
    if state.requested == family && state.effective == effective {
        return false;
    }

    let effective_changed = state.effective != effective;
    let state = cx.global_mut::<TypographyState>();
    state.requested = family.clone();
    state.effective = effective;

    if effective_changed {
        state.generation = state.generation.wrapping_add(1);
        crate::theme::bump_style_generation();
        let appearance = crate::theme::current_appearance();
        let themes = crate::appearance::themes(cx);
        crate::theme::Theme::install_selection(
            appearance,
            themes.variant_id(match appearance {
                crate::theme::Appearance::Dark => zeron_theme::Appearance::Dark,
                crate::theme::Appearance::Light => zeron_theme::Appearance::Light,
            }),
            crate::appearance::accent(cx),
            crate::appearance::surface(cx),
            cx,
        );
        cx.refresh_windows();
    }
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.ui_font_family = family;
    });
    effective_changed
}

/// Apply a supported UI size to the current window and persist it.
pub fn set_font_size(size: UiFontSize, window: &mut Window, cx: &mut App) -> bool {
    let size = size.normalized();
    let Some(state) = cx.try_global::<TypographyState>() else {
        return false;
    };
    if state.size == size {
        return false;
    }

    let state = cx.global_mut::<TypographyState>();
    state.size = size;
    state.generation = state.generation.wrapping_add(1);
    window.set_rem_size(px(size.pixels()));
    crate::theme::bump_style_generation();
    cx.refresh_windows();
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.ui_font_size = size;
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_values_round_trip_and_unknown_falls_back() {
        for family in [
            UiFontFamily::Geist,
            UiFontFamily::GeistMono,
            UiFontFamily::System,
            UiFontFamily::Installed("Helvetica Neue".into()),
        ] {
            let json = serde_json::to_string(&family).unwrap();
            assert_eq!(serde_json::from_str::<UiFontFamily>(&json).unwrap(), family);
        }
        assert_eq!(
            serde_json::from_str::<UiFontFamily>(r#""futureFont""#).unwrap(),
            UiFontFamily::Geist
        );
    }

    #[test]
    fn unavailable_family_resolves_to_geist() {
        let unavailable = UiFontFamily::Installed("Inter".into());
        let availability = FontAvailability::all().without(&unavailable);
        assert_eq!(
            resolve_effective(&unavailable, &availability),
            UiFontFamily::Geist
        );
        assert!(availability.is_available(&UiFontFamily::System));
    }

    #[test]
    fn catalog_keeps_bundled_and_virtual_choices_first() {
        let availability = FontAvailability::all();
        assert_eq!(
            availability.choices()[..3],
            [
                UiFontFamily::Geist,
                UiFontFamily::GeistMono,
                UiFontFamily::System
            ]
        );
    }

    #[test]
    fn legacy_optional_families_become_installed_choices() {
        assert_eq!(
            serde_json::from_str::<UiFontFamily>(r#""inter""#).unwrap(),
            UiFontFamily::Installed("Inter".into())
        );
        assert_eq!(
            serde_json::from_str::<UiFontFamily>(r#""atkinsonHyperlegibleNext""#).unwrap(),
            UiFontFamily::Installed("Atkinson Hyperlegible Next".into())
        );
    }

    #[test]
    fn latin_metric_filter_requires_the_glyph_gpui_measures() {
        let latin = ttf_parser::Face::parse(GEIST[0], 0).unwrap();
        assert!(latin.glyph_index('m').is_some());
    }

    #[test]
    fn ui_font_sizes_have_stable_labels_and_normalize() {
        assert_eq!(UiFontSize::default().label().as_ref(), "16 px");
        assert_eq!(UiFontSize(19).normalized(), UiFontSize(18));
        assert_eq!(UiFontSize(250).normalized(), UiFontSize(20));
        assert_eq!(ui_rems(14.0).0, 0.875);
    }

    #[test]
    fn bundled_families_have_required_static_faces() {
        for (expected_family, faces) in [
            ("Geist", GEIST.as_slice()),
            ("Geist Mono", GEIST_MONO.as_slice()),
        ] {
            let mut found = Vec::new();
            for bytes in faces {
                let face = ttf_parser::Face::parse(bytes, 0).unwrap();
                found.push((face.weight().to_number(), face.is_italic()));
                let has_family = face.names().into_iter().any(|name| {
                    name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY
                        && name.to_string().as_deref() == Some(expected_family)
                }) || face.names().into_iter().any(|name| {
                    name.name_id == ttf_parser::name_id::FAMILY
                        && name.to_string().as_deref() == Some(expected_family)
                });
                assert!(has_family, "wrong family metadata for {expected_family}");
            }
            found.sort_unstable();
            assert_eq!(
                found,
                vec![
                    (400, false),
                    (400, true),
                    (500, false),
                    (500, true),
                    (600, false),
                    (600, true),
                    (700, false),
                    (700, true),
                ],
                "missing static faces for {expected_family}"
            );
        }
    }
}
