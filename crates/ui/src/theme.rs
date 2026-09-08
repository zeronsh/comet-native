//! The app theme — two concrete appearances, one token set.
//!
//! Colors are precomputed from an oklch-derived neutral scale (perceptually even
//! lightness steps; the same scale zeron's Tailwind theme used) into gpui [`Hsla`].
//! **Numbers drive layout, colors are paint**: layout constants live here as plain
//! numbers and never depend on which color is painted.
//!
//! # Light is designed, not inverted
//!
//! Mirroring lightness produces the classic "washed-out inverted" look, for three
//! reasons this module handles explicitly:
//!
//! 1. **Surface order flips meaning.** In dark, the main content panel is the
//!    *darkest* plane and raised surfaces get *lighter*. In light, the content
//!    panel is *white* and the shell/sidebar goes *grey* — chrome recedes by
//!    getting darker, not lighter. Popovers stay white and earn separation from a
//!    border and shadow rather than from lightness.
//! 2. **Elevation reverses.** On dark, a faint *white* wash means "raised". Its
//!    literal translation — a faint *black* wash on white — means "recessed", so
//!    the composer read as a dent instead of a plate. Light lifts with white plus
//!    a border and shadow ([`Theme::input_bg`], the elevation ladder). Fill
//!    *alphas* carry over unchanged ([`INK_FILL_SCALE`]); only hairlines scale, so
//!    a 1px edge survives a bright surround ([`INK_HAIRLINE_SCALE`]).
//! 3. **Accents must move down the scale.** The dark palette's 400-level accents
//!    (indigo/red/amber) are chosen for contrast against near-black; on white they
//!    fall to 2–4:1 and fail WCAG AA. Light mode uses the 600-level siblings at the
//!    same hue, which restores the *contrast ratio* the dark token had.
//!
//! Text tones are chosen so each light token lands within ~0.5 of its dark
//! counterpart's contrast ratio against its own background — the pairing is
//! verified in [`tests::text_contrast_is_paired_across_appearances`], not eyeballed.
//!
//! Installed as a gpui [`Global`] at boot; read with [`Theme::of`].

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use gpui::{App, Global, Hsla, SharedString, hsla};
use serde::{Deserialize, Serialize};
use zeron_syntax::HighlightKind;
use zeron_theme::{
    AccentPreset, AccentSelection, Color as ModelColor, SurfacePreference, SurfaceTreatment,
    ThemeRegistry, ThemeVariant,
};

/// User-selectable accent family. A choice is one color identity, not a
/// miniature multi-hue theme: every interactive accent role stays on the same
/// authored hue in both appearances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccentColor {
    /// The exact upstream Zeron indigo.
    #[default]
    #[serde(alias = "violet", alias = "indigo", alias = "red", alias = "purple")]
    Zeron,
    Orange,
    Amber,
    Green,
    #[serde(alias = "teal")]
    Cyan,
    Blue,
    Pink,
}

impl AccentColor {
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

    fn tokens(self, appearance: Appearance) -> AccentTokens {
        // These are deliberately authored pairs. Runtime contrast correction
        // used to gamut-clip OKLCH into sRGB and then mutate HSL lightness,
        // producing different chroma and apparent hues across light/dark.
        let (primary, strong) = match (self, appearance) {
            (Self::Zeron, Appearance::Dark) => {
                (oklch(0.673, 0.182, 276.935), oklch(0.585, 0.233, 277.117))
            }
            (Self::Zeron, Appearance::Light) => {
                (oklch(0.511, 0.262, 276.966), oklch(0.511, 0.262, 276.966))
            }
            (Self::Orange, Appearance::Dark) => (oklch(0.75, 0.18, 55.0), oklch(0.54, 0.19, 55.0)),
            (Self::Orange, Appearance::Light) => (oklch(0.50, 0.19, 55.0), oklch(0.50, 0.19, 55.0)),
            (Self::Amber, Appearance::Dark) => (oklch(0.80, 0.17, 84.0), oklch(0.52, 0.14, 84.0)),
            (Self::Amber, Appearance::Light) => (oklch(0.48, 0.14, 84.0), oklch(0.48, 0.14, 84.0)),
            (Self::Green, Appearance::Dark) => (oklch(0.75, 0.17, 150.0), oklch(0.50, 0.15, 150.0)),
            (Self::Green, Appearance::Light) => {
                (oklch(0.46, 0.14, 150.0), oklch(0.46, 0.14, 150.0))
            }
            (Self::Cyan, Appearance::Dark) => (oklch(0.76, 0.13, 205.0), oklch(0.49, 0.12, 205.0)),
            (Self::Cyan, Appearance::Light) => (oklch(0.45, 0.11, 205.0), oklch(0.45, 0.11, 205.0)),
            (Self::Blue, Appearance::Dark) => (oklch(0.70, 0.17, 255.0), oklch(0.50, 0.20, 255.0)),
            (Self::Blue, Appearance::Light) => (oklch(0.47, 0.21, 255.0), oklch(0.47, 0.21, 255.0)),
            (Self::Pink, Appearance::Dark) => (oklch(0.72, 0.18, 350.0), oklch(0.51, 0.20, 350.0)),
            (Self::Pink, Appearance::Light) => (oklch(0.48, 0.20, 350.0), oklch(0.48, 0.20, 350.0)),
        };
        AccentTokens {
            primary,
            strong,
            wash: match appearance {
                Appearance::Dark => strong.opacity(0.45),
                Appearance::Light => primary.opacity(0.10),
            },
            selection: primary.opacity(if appearance.is_dark() { 0.35 } else { 0.24 }),
            caret: primary,
            code_text: primary,
            code_wash: primary.opacity(match appearance {
                Appearance::Dark => 0.12,
                Appearance::Light => 0.10,
            }),
            activity: primary,
            glyph: GlyphPalette::for_accent(primary, strong, appearance),
        }
    }
}

impl From<AccentColor> for AccentPreset {
    fn from(value: AccentColor) -> Self {
        match value {
            AccentColor::Zeron => Self::Zeron,
            AccentColor::Orange => Self::Orange,
            AccentColor::Amber => Self::Amber,
            AccentColor::Green => Self::Green,
            AccentColor::Cyan => Self::Cyan,
            AccentColor::Blue => Self::Blue,
            AccentColor::Pink => Self::Pink,
        }
    }
}

impl From<AccentPreset> for AccentColor {
    fn from(value: AccentPreset) -> Self {
        match value {
            AccentPreset::Zeron => Self::Zeron,
            AccentPreset::Orange => Self::Orange,
            AccentPreset::Amber => Self::Amber,
            AccentPreset::Green => Self::Green,
            AccentPreset::Cyan => Self::Cyan,
            AccentPreset::Blue => Self::Blue,
            AccentPreset::Pink => Self::Pink,
        }
    }
}

/// The three authored rows of the animated 2×3 pixel glyph. Keeping this as a
/// palette entity preserves the mark's light→mid→deep personality while letting
/// every accent preset own it as one coherent family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphPalette {
    pub light: Hsla,
    pub mid: Hsla,
    pub deep: Hsla,
}

impl GlyphPalette {
    fn for_accent(primary: Hsla, strong: Hsla, appearance: Appearance) -> Self {
        let mut light = primary;
        let mut deep = strong;
        match appearance {
            Appearance::Dark => {
                light.l = (light.l + 0.14).min(0.90);
                light.s *= 0.72;
            }
            Appearance::Light => {
                light.l = (light.l + 0.11).min(0.76);
                light.s *= 0.78;
                deep.l = (deep.l - 0.09).max(0.22);
            }
        }
        Self {
            light,
            mid: primary,
            deep,
        }
    }

    pub fn rows(self) -> [Hsla; 3] {
        [self.light, self.mid, self.deep]
    }
}

#[derive(Debug, Clone, Copy)]
struct AccentTokens {
    primary: Hsla,
    strong: Hsla,
    wash: Hsla,
    selection: Hsla,
    caret: Hsla,
    code_text: Hsla,
    code_wash: Hsla,
    activity: Hsla,
    glyph: GlyphPalette,
}

/// Which appearance the app is painting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    pub fn is_light(self) -> bool {
        matches!(self, Self::Light)
    }

    /// Map a gpui window appearance onto ours (both vibrant variants are just
    /// the blurred flavour of the same tone).
    pub fn from_window(appearance: gpui::WindowAppearance) -> Self {
        use gpui::WindowAppearance::*;
        match appearance {
            Light | VibrantLight => Self::Light,
            Dark | VibrantDark => Self::Dark,
        }
    }
}

/// Process-wide mirror of the installed theme's appearance.
///
/// The paint helpers ([`ink`], [`hairline`], [`wash`], …) are free functions
/// called from deep inside element builders that have no `cx` in scope, so they
/// read the appearance from here instead of the gpui global. Appearance is
/// genuinely process-wide — one setting for every window — so a single mirror is
/// sound; [`Theme::install`] is the only writer outside tests.
static CURRENT_APPEARANCE: AtomicU8 = AtomicU8::new(0);

/// Bumped every time the appearance actually changes.
///
/// Anything that caches *resolved colors* — most importantly the markdown
/// renderer's cross-frame `TextRun` cache, which bakes an `Hsla` into every run —
/// is only valid for the palette that produced it. Those caches were written when
/// the theme was a compile-time constant, so their validity keys cover content
/// only. Rather than thread the palette through every key, they compare this
/// counter and drop everything when it moves.
static STYLE_GENERATION: AtomicU32 = AtomicU32::new(0);

/// The appearance the context-free paint helpers are painting for.
pub fn current_appearance() -> Appearance {
    match CURRENT_APPEARANCE.load(Ordering::Relaxed) {
        1 => Appearance::Light,
        _ => Appearance::Dark,
    }
}

/// Monotonic id of the current resolved style (palette + UI typography).
pub fn style_generation() -> u32 {
    STYLE_GENERATION.load(Ordering::Relaxed)
}

/// Invalidate caches that bake resolved text styles.
pub(crate) fn bump_style_generation() {
    STYLE_GENERATION.fetch_add(1, Ordering::Relaxed);
}

fn model_appearance(appearance: zeron_theme::Appearance) -> Appearance {
    match appearance {
        zeron_theme::Appearance::Dark => Appearance::Dark,
        zeron_theme::Appearance::Light => Appearance::Light,
    }
}

fn model_color(color: ModelColor) -> Hsla {
    let (h, s, l) = rgb_to_hsl(
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
    );
    hsla(h, s, l, color.a as f32 / 255.0)
}

fn harden_model_foreground(
    color: ModelColor,
    backgrounds: &[ModelColor],
    minimum: f32,
    preferred_target: Option<ModelColor>,
) -> ModelColor {
    let minimum_contrast = |candidate: ModelColor| {
        backgrounds
            .iter()
            .map(|background| candidate.contrast(*background))
            .fold(f32::INFINITY, f32::min)
    };
    if minimum_contrast(color) >= minimum {
        return color;
    }
    let mut targets = Vec::with_capacity(3);
    if let Some(target) = preferred_target {
        targets.push(target);
    }
    targets.extend([ModelColor::BLACK, ModelColor::WHITE]);
    let mut best = color;
    let mut best_contrast = minimum_contrast(color);
    for target in targets {
        for step in 1..=100 {
            let candidate = color.mix(target, step as f32 / 100.0);
            let contrast = minimum_contrast(candidate);
            if contrast > best_contrast {
                best = candidate;
                best_contrast = contrast;
            }
            if contrast >= minimum {
                return candidate;
            }
        }
    }
    best
}

/// [`CURRENT_APPEARANCE`] is process-wide, so under the parallel test runner
/// any test that flips it — or asserts on the output of a helper that reads it
/// ([`ink`], [`hairline`], [`wash`], …) — must hold this lock. Crate-visible
/// because such tests exist outside this module too (see `motion::tests`).
/// Tests that flip the appearance restore Dark before releasing the guard.
#[cfg(test)]
pub(crate) fn lock_appearance() -> std::sync::MutexGuard<'static, ()> {
    static APPEARANCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    APPEARANCE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Point the context-free paint helpers at an appearance. Called by
/// [`Theme::install`]; exposed for tests that build a theme without an `App`.
pub fn set_current_appearance(appearance: Appearance) {
    let encoded = match appearance {
        Appearance::Dark => 0,
        Appearance::Light => 1,
    };
    if CURRENT_APPEARANCE.swap(encoded, Ordering::Relaxed) != encoded {
        bump_style_generation();
    }
}

/// Light-mode alpha multiplier for **fills** (hover/active washes, chip and pill
/// backgrounds).
///
/// This was 0.5 on the theory that dark ink on a bright field reads heavier and
/// should be scaled back. That theory is right for a *large* wash and badly wrong
/// for everything else: this palette leans on very low alphas for its subtle
/// fills — the composer plate is `ink(0.03)`, key caps are `ink(0.05)` — and
/// halving those produced 1.5% black on white, which is nothing. The composer
/// lost its background entirely and selected tabs stopped reading as selected.
///
/// The established light-UI scales (Primer, Radix) land subtle ≈ 3–4%, hover ≈ 8%,
/// selected ≈ 14% black — which is where the dark palette's white alphas already
/// sit. So the honest multiplier is 1: the same number in both appearances, with
/// only the *tone* flipping. Any per-state correction belongs in that state's
/// token, not in a blanket multiplier.
pub const INK_FILL_SCALE: f32 = 1.0;

/// Light-mode alpha multiplier for **hairlines** (borders, dividers, rings).
/// Opposite of fills: a 1px edge has to hold its own against a bright surround,
/// and the dark palette's white hairlines are deliberately faint. Scaling up
/// keeps separators legible instead of dissolving into the panel.
pub const INK_HAIRLINE_SCALE: f32 = 1.35;

/// Paint-only syntax colors. The hues follow the Git history graph's lane
/// palette (indigo, pink, emerald, amber, red, neutral), while light-mode
/// variants are darkened enough to remain readable as text on white.
#[derive(Debug, Clone)]
pub struct SyntaxPalette {
    pub comment: Hsla,
    pub keyword: Hsla,
    pub string: Hsla,
    pub string_special: Hsla,
    pub escape: Hsla,
    pub number: Hsla,
    pub boolean: Hsla,
    pub type_name: Hsla,
    pub type_builtin: Hsla,
    pub constructor: Hsla,
    pub function: Hsla,
    pub function_builtin: Hsla,
    pub macro_name: Hsla,
    pub property: Hsla,
    pub constant: Hsla,
    pub variable: Hsla,
    pub variable_special: Hsla,
    pub parameter: Hsla,
    pub operator: Hsla,
    pub punctuation: Hsla,
    pub tag: Hsla,
    pub attribute: Hsla,
    pub label: Hsla,
    pub markup_heading: Hsla,
    pub markup_raw: Hsla,
    pub markup_link: Hsla,
    pub markup_reference: Hsla,
    pub markup_emphasis: Hsla,
    pub markup_strong: Hsla,
    pub invalid: Hsla,
}

impl SyntaxPalette {
    pub fn color(&self, kind: HighlightKind) -> Hsla {
        match kind {
            HighlightKind::Comment => self.comment,
            HighlightKind::Keyword => self.keyword,
            HighlightKind::String => self.string,
            HighlightKind::StringSpecial => self.string_special,
            HighlightKind::Escape => self.escape,
            HighlightKind::Number => self.number,
            HighlightKind::Boolean => self.boolean,
            HighlightKind::Type => self.type_name,
            HighlightKind::TypeBuiltin => self.type_builtin,
            HighlightKind::Constructor => self.constructor,
            HighlightKind::Function => self.function,
            HighlightKind::FunctionBuiltin => self.function_builtin,
            HighlightKind::Macro => self.macro_name,
            HighlightKind::Property => self.property,
            HighlightKind::Constant => self.constant,
            HighlightKind::Variable => self.variable,
            HighlightKind::VariableSpecial => self.variable_special,
            HighlightKind::Parameter => self.parameter,
            HighlightKind::Operator => self.operator,
            HighlightKind::Punctuation | HighlightKind::Embedded => self.punctuation,
            HighlightKind::Tag => self.tag,
            HighlightKind::Attribute => self.attribute,
            HighlightKind::Label => self.label,
            HighlightKind::MarkupHeading => self.markup_heading,
            HighlightKind::MarkupRaw => self.markup_raw,
            HighlightKind::MarkupLink => self.markup_link,
            HighlightKind::MarkupReference => self.markup_reference,
            HighlightKind::MarkupEmphasis => self.markup_emphasis,
            HighlightKind::MarkupStrong => self.markup_strong,
            HighlightKind::Invalid => self.invalid,
        }
    }

    fn from_variant(variant: &ThemeVariant, fallback: Self) -> Self {
        let color = |key: &str, fallback: Hsla| {
            variant
                .syntax
                .get(key)
                .copied()
                .map(model_color)
                .unwrap_or(fallback)
        };
        Self {
            comment: color("comment", fallback.comment),
            keyword: color("keyword", fallback.keyword),
            string: color("string", fallback.string),
            string_special: color("stringSpecial", fallback.string_special),
            escape: color("escape", fallback.escape),
            number: color("number", fallback.number),
            boolean: color("boolean", fallback.boolean),
            type_name: color("type", fallback.type_name),
            type_builtin: color("typeBuiltin", fallback.type_builtin),
            constructor: color("constructor", fallback.constructor),
            function: color("function", fallback.function),
            function_builtin: color("functionBuiltin", fallback.function_builtin),
            macro_name: color("macro", fallback.macro_name),
            property: color("property", fallback.property),
            constant: color("constant", fallback.constant),
            variable: color("variable", fallback.variable),
            variable_special: color("variableSpecial", fallback.variable_special),
            parameter: color("parameter", fallback.parameter),
            operator: color("operator", fallback.operator),
            punctuation: color("punctuation", fallback.punctuation),
            tag: color("tag", fallback.tag),
            attribute: color("attribute", fallback.attribute),
            label: color("label", fallback.label),
            markup_heading: color("markupHeading", fallback.markup_heading),
            markup_raw: color("markupRaw", fallback.markup_raw),
            markup_link: color("markupLink", fallback.markup_link),
            markup_reference: color("markupReference", fallback.markup_reference),
            markup_emphasis: color("markupEmphasis", fallback.markup_emphasis),
            markup_strong: color("markupStrong", fallback.markup_strong),
            invalid: color("invalid", fallback.invalid),
        }
    }

    fn dark(text: Hsla, comment: Hsla, danger: Hsla) -> Self {
        // Same sources and 72% saturation treatment as history::graph_color.
        let indigo = git_graph_tone(oklch(0.673, 0.182, 276.935));
        let pink = git_graph_tone(oklch(0.718, 0.202, 349.761));
        let emerald = git_graph_tone(oklch(0.765, 0.177, 163.223));
        let amber = git_graph_tone(oklch(0.828, 0.189, 84.429));
        let red = git_graph_tone(danger);
        Self {
            comment,
            keyword: indigo,
            string: emerald,
            string_special: pink,
            escape: pink,
            number: amber,
            boolean: amber,
            type_name: amber,
            type_builtin: emerald,
            constructor: amber,
            function: indigo,
            function_builtin: pink,
            macro_name: pink,
            property: amber,
            constant: emerald,
            variable: text,
            variable_special: pink,
            parameter: text,
            operator: text,
            punctuation: text,
            tag: pink,
            attribute: amber,
            label: amber,
            markup_heading: indigo,
            markup_raw: emerald,
            markup_link: pink,
            markup_reference: amber,
            markup_emphasis: pink,
            markup_strong: indigo,
            invalid: red,
        }
    }

    fn light(text: Hsla, comment: Hsla, danger: Hsla) -> Self {
        // Match the light graph's hue families at text-safe lightness.
        let indigo = git_graph_tone(oklch(0.47, 0.20, 276.966));
        let pink = git_graph_tone(oklch(0.47, 0.17, 0.584));
        let emerald = git_graph_tone(oklch(0.46, 0.11, 163.225));
        let amber = git_graph_tone(oklch(0.47, 0.12, 48.998));
        let red = git_graph_tone(danger);
        Self {
            comment,
            keyword: indigo,
            string: emerald,
            string_special: pink,
            escape: pink,
            number: amber,
            boolean: amber,
            type_name: amber,
            type_builtin: emerald,
            constructor: amber,
            function: indigo,
            function_builtin: pink,
            macro_name: pink,
            property: amber,
            constant: emerald,
            variable: text,
            variable_special: pink,
            parameter: text,
            operator: text,
            punctuation: text,
            tag: pink,
            attribute: amber,
            label: amber,
            markup_heading: indigo,
            markup_raw: emerald,
            markup_link: pink,
            markup_reference: amber,
            markup_emphasis: pink,
            markup_strong: indigo,
            invalid: red,
        }
    }

    fn for_appearance(appearance: Appearance, text: Hsla, comment: Hsla, danger: Hsla) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(text, comment, danger),
            Appearance::Light => Self::light(text, comment, danger),
        }
    }
}

/// Git history intentionally softens lane saturation so the graph remains
/// colorful without competing with content. Syntax uses the same treatment.
fn git_graph_tone(mut color: Hsla) -> Hsla {
    color.s *= 0.72;
    color
}

/// The app theme. Two concrete instances — [`Theme::dark`] and [`Theme::light`].
#[derive(Debug, Clone)]
pub struct Theme {
    /// Which appearance these tokens were built for.
    pub appearance: Appearance,
    /// Stable id of the resolved theme variant.
    pub variant_id: SharedString,
    /// Stable id of the family that owns [`Self::variant_id`].
    pub family_id: SharedString,
    /// Whether the base theme or a user preset owns interactive identity.
    pub accent_selection: AccentSelection,
    /// The persisted policy that resolved [`Self::surface_treatment`].
    pub surface_preference: SurfacePreference,
    /// The effective treatment after applying [`Self::surface_preference`] to
    /// the selected variant's recommendation.
    pub surface_treatment: SurfaceTreatment,
    /// The selected interactive accent used to build this theme.
    pub accent_color: AccentColor,

    // ---- paint: neutral surfaces ----
    /// Main content panel. Dark: the deepest plane (#060606). Light: pure white —
    /// long-form content reads best on an unbroken white field.
    pub bg: Hsla,
    /// Shell / sidebar surface. Dark: one step *up* from `bg`. Light: one step
    /// *down* (grey) — chrome recedes from the content plane in both, which is
    /// the direction a naive invert gets backwards.
    pub surface: Hsla,
    /// Raised surface: opaque pills and chips that sit proud of the panel.
    /// Dark: lighter than `surface`. Light: white, separated by `border` +
    /// shadow rather than by lightness.
    pub surface_raised: Hsla,

    // ---- paint: elevation ladder ----
    //
    // Dark mode distinguishes floating planes by lightness, and the steps are
    // *small* (#0e → #10 → #16 → #1e). They are not interchangeable: collapsing
    // them onto one token visibly lifts popovers off their intended plane.
    //
    // Light mode cannot use the same trick, because the content plane is already
    // white and there is nothing lighter to climb to. All three land on white and
    // let `border` + shadow carry the separation instead — the standard light-UI
    // answer, and the reason this is a ladder of tokens rather than an arithmetic
    // offset applied to one.
    /// Inline card resting on the main panel (auth gate, empty-state cards).
    pub surface_card: Hsla,
    /// Modal dialog, floating over a [`Theme::scrim`].
    pub surface_dialog: Hsla,
    /// Popover, menu and command-palette surface — the highest plane.
    pub surface_overlay: Hsla,
    /// Hover wash for interactive rows/buttons.
    pub element_hover: Hsla,
    /// Active/selected wash.
    pub element_active: Hsla,
    /// Hairline border.
    pub border: Hsla,
    /// Stronger border for focused/raised edges.
    pub border_strong: Hsla,

    // ---- paint: text ----
    /// Primary text. ~17.5:1 on its own background in both appearances.
    pub text: Hsla,
    /// Muted text: timestamps, secondary labels. ~7.5–8:1.
    pub text_muted: Hsla,
    /// Faint text: placeholders, disabled. ~4.5:1 — AA for body copy.
    pub text_faint: Hsla,
    /// One notch below `text_muted` — the diff file-path tone. It exists as its
    /// own token rather than being folded into `text_muted` because the dark
    /// value was sampled (#989898) and folding it would shift that label, which
    /// is a palette change dressed up as a refactor.
    pub text_dim: Hsla,

    // ---- paint: high-contrast solid (primary buttons) ----
    /// The maximum-contrast solid fill: near-white on dark, near-black on light.
    /// This is the primary button plate.
    pub solid: Hsla,
    /// Label/icon color on top of [`Self::solid`] — its inverse.
    pub on_solid: Hsla,

    // ---- paint: accents ----
    /// Primary tone in the selected accent family.
    pub accent: Hsla,
    /// Stronger accent for fills that carry [`Self::on_accent`] text.
    pub accent_strong: Hsla,
    /// Low-emphasis wash of the same accent for tinted identity surfaces.
    pub accent_wash: Hsla,
    /// Label color on top of [`Self::accent_strong`].
    pub on_accent: Hsla,
    /// Danger — red (errors, stop button).
    pub danger: Hsla,
    /// Softer danger for secondary/inline error copy.
    pub danger_muted: Hsla,
    /// Warning — amber (offline notices, awaiting-input).
    pub warning: Hsla,
    /// Softer warning for secondary copy.
    pub warning_muted: Hsla,
    /// Success / online — emerald.
    pub success: Hsla,
    /// Working / streaming indicator in the selected accent family.
    pub busy: Hsla,
    /// Three-tone animated pixel glyph palette in the selected accent family.
    pub glyph: GlyphPalette,
    /// Softer success for text on a success-tinted chip.
    pub success_muted: Hsla,

    // ---- paint: components ----
    /// Hover tone for an *opaque* raised pill. Hover must brighten the plate in
    /// dark mode, never swap it for a translucent wash (that made pills go
    /// see-through — user-reported); in light mode it darkens instead, same idea.
    pub surface_raised_hover: Hsla,
    /// Recessed band behind a palette/picker header or footer strip. Translucent
    /// so the glass still reads through.
    pub band: Hsla,
    /// The composer pill and other input plates.
    ///
    /// Its own token because "lifted" inverts between appearances. On dark, a
    /// faint *white* wash over near-black reads as raised. The literal light
    /// translation — a faint *black* wash on white — reads as **recessed**, a dent
    /// rather than a plate, which is why the prompt looked like bare text on a
    /// smudge. Light mode lifts the way light UIs actually do: pure white, with
    /// the border and shadow carrying the elevation.
    pub input_bg: Hsla,
    /// Text-selection highlight in the composer and inputs.
    pub selection: Hsla,
    /// Terminal block cursor.
    pub cursor: Hsla,
    /// Composer text caret in the selected accent family.
    pub caret: Hsla,
    /// Destructive-action button fill (danger plate, carries [`Self::on_accent`]).
    pub danger_strong: Hsla,

    // ---- paint: code & diff ----
    /// Inline-code text in the selected accent family.
    pub code_text: Hsla,
    /// Inline-code wash behind [`Self::code_text`].
    pub code_wash: Hsla,
    /// Shared paint-only syntax palette.
    pub syntax: SyntaxPalette,
    /// Diff: added lines.
    pub diff_add: Hsla,
    /// Diff: deleted lines.
    pub diff_del: Hsla,
    /// Diff: hunk-header wash (bluish grey).
    pub diff_hunk_bg: Hsla,

    /// Theme-owned terminal background, selection, and ANSI16 palette.
    pub terminal: TerminalColors,

    // ---- fonts ----
    /// UI font family (bundling of Geist lands with asset work; until then the
    /// text system falls back to the system sans when the family is missing).
    pub font_sans: SharedString,
    /// Fixed Geist chrome for code-adjacent surfaces and recovery controls.
    pub font_sans_fixed: SharedString,
    /// Monospace family for code/terminal.
    pub font_mono: SharedString,
    /// Explicit system fallbacks, for callers that want to skip the lookup.
    pub font_sans_fallback: SharedString,
    pub font_mono_fallback: SharedString,
}

#[derive(Debug, Clone)]
pub struct TerminalColors {
    pub background: Hsla,
    pub foreground: Hsla,
    pub selection: Hsla,
    pub ansi: [Hsla; 16],
}

impl TerminalColors {
    fn from_variant(variant: &ThemeVariant) -> Self {
        Self {
            background: model_color(variant.terminal.background),
            foreground: model_color(variant.terminal.foreground),
            selection: model_color(variant.terminal.selection),
            ansi: variant.terminal.ansi.map(model_color),
        }
    }

    fn zeron(appearance: Appearance) -> Self {
        let id = match appearance {
            Appearance::Dark => "zeron-dark",
            Appearance::Light => "zeron-light",
        };
        let registry = ThemeRegistry::active();
        Self::from_variant(registry.variant(id).expect("Zeron terminal palette exists"))
    }
}

impl Theme {
    // ---- numbers drive layout (px) ----
    /// Frost translucency over the blurred window background (macOS vibrancy).
    /// Opaque elsewhere: Linux/Windows get no compositor-blur guarantee, and a
    /// merely transparent window would show raw desktop through the sidebar.
    /// Darkness matched by eye to a reference Electron app's dark glass. That
    /// scrim is 0.76 over `hsl(0 0% 3%)`, but it sits on Electron's
    /// `under-window` vibrancy MATERIAL, which pre-darkens the blur; our bare
    /// backdrop blur has no material layer, so the scrim runs heavier to land
    /// on the same perceived tone (see [`Theme::glass`]).
    pub const GLASS_ALPHA: f32 = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    /// Light-mode frost alpha — glass-forward, like dark mode.
    ///
    /// A light tint controls the blur less than a dark one: the desktop's
    /// colour bleeds through more readily, so light frost runs *heavier* than
    /// an equal-looking dark frost to keep the chrome on a known-enough
    /// background for its labels (macOS light sidebars do the same — their
    /// vibrancy material is mostly white). Floating cards compensate further:
    /// see [`Self::glass_overlay`], where light coverage steps up to keep menu
    /// text legible over an unknown backdrop.
    pub const GLASS_ALPHA_LIGHT: f32 = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    /// Main-panel header height (zeron `h-11`) — in-card headers (changes pane).
    pub const HEADER_HEIGHT: f32 = 44.0;
    /// The unified window titlebar (traffic lights + cluster + tabs). Content
    /// rides [`Self::TITLEBAR_TOP_PAD`] lower than center so the air above
    /// matches the perceived gap to the inset card below (border + card body).
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Downward shift of titlebar content within the bar.
    pub const TITLEBAR_TOP_PAD: f32 = 2.0;
    /// Reserved status strip under the content outlet (zeron `h-6`) — the
    /// WorkingIndicator row; reserving it keeps the composer from shifting.
    pub const STATUS_STRIP_HEIGHT: f32 = 24.0;
    /// Height of the gradient that fades the transcript into the panel
    /// background at its bottom edge. The transcript's last row must pad
    /// itself past this band so settled content (message text, the
    /// hover-revealed timestamp) never sits inside the fade when scrolled
    /// to the bottom.
    pub const TRANSCRIPT_FADE_BAND: f32 = 24.0;
    /// Message bubble corner radius.
    pub const BUBBLE_RADIUS: f32 = 16.0;
    /// Panel / card corner radius.
    pub const PANEL_RADIUS: f32 = 10.0;
    /// Small control radius (buttons, chips).
    pub const CONTROL_RADIUS: f32 = 6.0;
    /// Base spacing steps.
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;
    /// Optical separation for a tightly coupled title/description stack.
    /// This is intentionally outside the base spacing ladder: it corrects
    /// line-box whitespace rather than separating layout regions.
    pub const TEXT_STACK_GAP: f32 = 1.0;

    /// The selected theme's shell tint painted over the blurred window
    /// background (macOS glass). Keeping the hue theme-owned matters when a
    /// user forces frost onto a palette authored for an opaque workbench: a
    /// fixed Zeron grey would erase that palette's identity.
    pub fn glass(&self) -> Hsla {
        if self.surface_treatment == SurfaceTreatment::Opaque {
            return self.surface;
        }
        let base = match self.appearance {
            Appearance::Dark => Self::GLASS_ALPHA,
            Appearance::Light => Self::GLASS_ALPHA_LIGHT,
        };
        self.surface
            .opacity(self.contrast_checked_glass_alpha(base))
    }

    /// Raise a requested tint's coverage until primary and muted shell text
    /// remain legible against the adverse desktop luminance for this
    /// appearance. A theme with unusually delicate contrast may therefore get
    /// denser glass, never silently broken text.
    fn contrast_checked_glass_alpha(&self, base: f32) -> f32 {
        self.contrast_checked_tint_alpha(self.surface, base, self.adverse_backdrop())
    }

    /// Increase tint coverage only as far as needed for Zeron's shared text
    /// roles. This is used for both window glass and in-app frosted surfaces,
    /// whose blurred content can otherwise invalidate an imported palette's
    /// original solid-background assumptions.
    fn contrast_checked_tint_alpha(&self, tint: Hsla, base: f32, backdrop: Hsla) -> f32 {
        for step in 0..=20 {
            let alpha = base + (1.0 - base) * step as f32 / 20.0;
            let composite = flatten(tint.opacity(alpha), backdrop);
            if painted_contrast(self.text, composite) >= 4.5
                && painted_contrast(self.text_muted, composite) >= 3.0
            {
                return alpha;
            }
        }
        1.0
    }

    fn adverse_backdrop(&self) -> Hsla {
        match self.appearance {
            Appearance::Dark => grey(0xff),
            Appearance::Light => grey(0),
        }
    }

    /// Whether this appearance paints translucent chrome over the blurred
    /// desktop. Glass-only recipes — backdrop blurs, translucent popover
    /// tints, per-glyph edge fades — must gate on this, not on
    /// [`Self::GLASS_ALPHA`]: that constant is platform-wide, while the frost
    /// alpha (and with it whether glass is on at all) is per-appearance.
    pub fn is_glass(&self) -> bool {
        self.glass().a < 1.0
    }

    /// Whether FLOATING surfaces (popovers, the composer pill) paint their
    /// backdrop blur and translucent tints. Unlike [`Self::is_glass`] this is
    /// scene-level: the blur runs on in-app content inside the window, not on
    /// the desktop behind it, so it needs no compositor vibrancy — macOS
    /// rasterizes it in Metal and Linux in the vendored wgpu renderer (other
    /// wgpu platforms keep opaque floats until tested). The window chrome
    /// itself stays opaque off macOS either way.
    pub fn is_frost(&self) -> bool {
        self.surface_treatment == SurfaceTreatment::Frosted
            && cfg!(any(target_os = "macos", target_os = "linux"))
    }

    /// Theme-owned hover wash for chrome that sits on glass (sidebar rows,
    /// tabs, titlebar buttons). The importer maps this role from the source
    /// theme, so forcing frost does not reintroduce Zeron's neutral hover.
    pub fn glass_hover(&self) -> Hsla {
        self.element_hover
    }

    /// The theme-owned tint floating cards paint over their backdrop blur (see
    /// [`crate::frost::frosted`]). Light coverage stays heavier because dark
    /// text is more vulnerable to unpredictable content behind a popover.
    pub fn glass_overlay(&self) -> Hsla {
        let base = match self.appearance {
            Appearance::Dark => self.surface_overlay.opacity(0.50),
            Appearance::Light => self.surface_overlay.opacity(0.85),
        };
        if !self.is_frost() {
            return self.surface_overlay;
        }
        self.surface_overlay
            .opacity(self.contrast_checked_tint_alpha(
                self.surface_overlay,
                base.a,
                self.adverse_backdrop(),
            ))
    }

    /// The composer pill / question panel fill. Light's `input_bg` is opaque
    /// white (the elevation ladder on an opaque page) — over glass it read as
    /// a solid slab in front of the frosted blur, so it thins to a
    /// translucent tint there (0.6 and then 0.45 both still read too bright
    /// over the 0.80 frost — lowered on user request). Dark's 3% white wash
    /// is already glass-native.
    pub fn input_glass_bg(&self) -> Hsla {
        if !self.is_frost() {
            return self.input_bg;
        }
        let base = if matches!(self.appearance, Appearance::Light) {
            0.30
        } else {
            self.input_bg.a
        };
        let window = flatten(self.glass(), self.adverse_backdrop());
        self.input_bg
            .opacity(self.contrast_checked_tint_alpha(self.input_bg, base, window))
    }

    /// Section-card fill (settings cards and similar in-panel cards). The
    /// opaque `surface` tone read as a harsh solid slab floating on the
    /// frosted blur (user report), so glass thins it to a translucent tint;
    /// opaque platforms keep the true card tone.
    pub fn card_glass_bg(&self) -> Hsla {
        if !self.is_frost() {
            return self.surface;
        }
        let window = flatten(self.glass(), self.adverse_backdrop());
        self.surface
            .opacity(self.contrast_checked_tint_alpha(self.surface, 0.40, window))
    }

    /// The standard modal backdrop — see [`scrim`].
    pub fn scrim(&self) -> Hsla {
        scrim_for(self.appearance, SCRIM_ALPHA_DARK)
    }

    /// How the platform should composite the window behind our paint.
    ///
    /// Only dark macOS wants the blurred desktop — light chrome is opaque by
    /// design ([`Self::GLASS_ALPHA_LIGHT`]), so it keeps opaque compositing
    /// (subpixel-friendly, no vibrancy cost for a blur nothing shows). This is
    /// a method rather than a constant because it has to be *re-applied* after
    /// every theme swap: gpui's macOS backend tears the `NSVisualEffectView`
    /// out of the hierarchy whenever the value is anything but `Blurred`, and
    /// the re-apply in `appearance::apply` is what restores vibrancy when the
    /// user switches back to dark. See zed's `crates/zed/src/main.rs`, which
    /// runs the same loop on every settings change.
    pub fn window_background_appearance(&self) -> gpui::WindowBackgroundAppearance {
        if self.is_glass() {
            gpui::WindowBackgroundAppearance::Blurred
        } else {
            gpui::WindowBackgroundAppearance::Opaque
        }
    }

    /// Build the dark theme. The surface tones are sampled straight from the
    /// reference screenshots of the original app (docs/reference): main panel
    /// `#060606`, shell/sidebar `#0d0d0d`.
    pub fn dark() -> Self {
        Self::dark_with_accent(AccentColor::default())
    }

    pub fn dark_with_accent(accent_color: AccentColor) -> Self {
        let accent = accent_color.tokens(Appearance::Dark);
        Self {
            appearance: Appearance::Dark,
            variant_id: "zeron-dark".into(),
            family_id: "zeron".into(),
            accent_selection: AccentSelection::Preset(accent_color.into()),
            surface_preference: SurfacePreference::ThemeDefault,
            surface_treatment: SurfaceTreatment::Frosted,
            accent_color,
            bg: grey(6),       // main panel — sampled #060606
            surface: grey(13), // shell / sidebar — sampled #0d0d0d
            surface_raised: neutral(0.235),
            surface_card: grey(0x0e),
            surface_dialog: grey(0x10),
            surface_overlay: grey(0x16),
            element_hover: hsla(0.0, 0.0, 0.92, 0.11),
            element_active: hsla(0.0, 0.0, 0.92, 0.16),
            border: hsla(0.0, 0.0, 1.0, 0.08),
            border_strong: hsla(0.0, 0.0, 1.0, 0.14),
            text: neutral(0.922),       // ~neutral-200
            text_muted: neutral(0.708), // ~neutral-400
            text_faint: neutral(0.556), // ~neutral-500
            text_dim: grey(0x98),
            solid: neutral(0.922), // near-white plate
            on_solid: grey(0x0e),  // near-black label
            accent: accent.primary,
            accent_strong: accent.strong,
            accent_wash: accent.wash,
            on_accent: neutral(0.985),
            danger: oklch(0.704, 0.191, 22.216),       // red-400
            danger_muted: oklch(0.808, 0.114, 19.571), // red-300
            warning: oklch(0.828, 0.189, 84.429),      // amber-400
            warning_muted: oklch(0.924, 0.12, 95.746), // amber-200
            success: oklch(0.765, 0.177, 163.223),     // emerald-400
            busy: accent.activity,
            glyph: accent.glyph,
            success_muted: oklch(0.845, 0.143, 164.978), // emerald-300
            surface_raised_hover: neutral(0.29),
            band: band_for(Appearance::Dark),
            input_bg: hsla(0.0, 0.0, 1.0, 0.03),
            selection: accent.selection,
            cursor: hsla(0.0, 0.0, 1.0, 0.35),
            caret: accent.caret,
            danger_strong: oklch(0.58, 0.16, 25.0),
            code_text: accent.code_text,
            code_wash: accent.code_wash,
            syntax: SyntaxPalette::for_appearance(
                Appearance::Dark,
                neutral(0.922),
                neutral(0.60),
                oklch(0.704, 0.191, 22.216),
            ),
            diff_add: oklch(0.765, 0.177, 163.223), // emerald-400
            diff_del: oklch(0.704, 0.191, 22.216),  // red-400
            diff_hunk_bg: hsla(0.6, 0.35, 0.6, 0.05),
            terminal: TerminalColors::zeron(Appearance::Dark),
            font_sans: "Geist".into(),
            font_sans_fixed: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// Build the light theme.
    ///
    /// Neutrals are the same oklch scale read from the other end, but the *roles*
    /// are reassigned rather than mirrored (see the module docs): content plane
    /// white, chrome grey, raised surfaces white-plus-shadow. Text tones are
    /// picked to reproduce the dark theme's contrast ratios, and accents drop
    /// from the 400 to the 600 step at identical hue so they clear WCAG AA on
    /// white instead of glowing.
    pub fn light() -> Self {
        Self::light_with_accent(AccentColor::default())
    }

    pub fn light_with_accent(accent_color: AccentColor) -> Self {
        let accent = accent_color.tokens(Appearance::Light);
        Self {
            appearance: Appearance::Light,
            variant_id: "zeron-light".into(),
            family_id: "zeron".into(),
            accent_selection: AccentSelection::Preset(accent_color.into()),
            surface_preference: SurfacePreference::ThemeDefault,
            surface_treatment: SurfaceTreatment::Frosted,
            accent_color,
            bg: grey(0xff), // main panel — clean white
            // Deeper than ~neutral-100 looks on paper: the content card is pure
            // white and sits *inside* this surface, so too small a step leaves the
            // whole window one flat sheet with a hairline drawn on it.
            surface: neutral(0.968),
            // A real grey, NOT white. This is the opaque-plate tone — user
            // message bubbles, the jump-to-bottom pill — and those sit directly
            // on the white content plane with no border or shadow to save them.
            // White here made the user's own messages vanish into the page.
            // Popovers do not use this; they have their own ladder below.
            surface_raised: neutral(0.940),
            surface_card: grey(0xff),
            surface_dialog: grey(0xff),
            surface_overlay: grey(0xff),
            element_hover: hsla(0.0, 0.0, 0.10, 0.06),
            element_active: hsla(0.0, 0.0, 0.10, 0.10),
            border: hsla(0.0, 0.0, 0.0, 0.10),
            border_strong: hsla(0.0, 0.0, 0.0, 0.17),
            // ~neutral-850. Pure neutral-900 measures 17.9:1 on white — *more*
            // contrast than dark mode's 16.1:1, which reads as harsh rather than
            // crisp. Backing off to 0.25 lands at ~16:1: the same perceived
            // weight as the dark theme, not the maximum available.
            text: neutral(0.25),
            text_muted: neutral(0.439), // ~neutral-600 → ~7.7:1
            // A touch darker than dark mode's neutral-500 counterpart: the light
            // sidebar is a real grey, and faint text has to clear its floor there
            // too, not just on the white content plane.
            text_faint: neutral(0.535),
            text_dim: neutral(0.50),
            solid: neutral(0.205),    // near-black plate, deeper than body text
            on_solid: neutral(0.985), // near-white label
            accent: accent.primary,
            accent_strong: accent.strong,
            accent_wash: accent.wash,
            on_accent: neutral(0.985),
            danger: oklch(0.577, 0.245, 27.325),        // red-600
            danger_muted: oklch(0.505, 0.213, 27.518),  // red-700
            warning: oklch(0.555, 0.163, 48.998),       // amber-700 — carries 12px text
            warning_muted: oklch(0.473, 0.137, 46.201), // amber-800
            success: oklch(0.596, 0.145, 163.225),      // emerald-600
            busy: accent.activity,
            glyph: accent.glyph,
            success_muted: oklch(0.508, 0.118, 165.612), // emerald-700
            // Opaque pills darken on hover here rather than brighten — same
            // "brighten the plate, don't wash it out" rule, read the other way.
            surface_raised_hover: neutral(0.900),
            // A recessed strip on white needs far less ink than on near-black;
            // the dark 16% would read as a bruise.
            band: band_for(Appearance::Light),
            input_bg: grey(0xff),
            selection: accent.selection,
            cursor: hsla(0.0, 0.0, 0.0, 0.55),
            caret: accent.caret,
            danger_strong: oklch(0.51, 0.20, 25.0),
            code_text: accent.code_text,
            code_wash: accent.code_wash,
            syntax: SyntaxPalette::for_appearance(
                Appearance::Light,
                neutral(0.25),
                neutral(0.48),
                oklch(0.505, 0.213, 27.518),
            ),
            diff_add: oklch(0.596, 0.145, 163.225), // emerald-600
            diff_del: oklch(0.577, 0.245, 27.325),  // red-600
            diff_hunk_bg: hsla(0.6, 0.35, 0.35, 0.07),
            terminal: TerminalColors::zeron(Appearance::Light),
            font_sans: "Geist".into(),
            font_sans_fixed: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// Build the theme for an appearance.
    pub fn for_appearance(appearance: Appearance) -> Self {
        Self::for_preferences(appearance, AccentColor::default())
    }

    pub fn for_preferences(appearance: Appearance, accent: AccentColor) -> Self {
        match appearance {
            Appearance::Dark => Self::dark_with_accent(accent),
            Appearance::Light => Self::light_with_accent(accent),
        }
    }

    fn with_font_sans(mut self, family: SharedString) -> Self {
        self.font_sans = family;
        self
    }

    /// Resolve one complete registered variant and then apply the narrowly
    /// scoped accent policy. Components consume only the resulting semantic
    /// tokens; VS Code keys never escape the importer.
    pub fn for_selection(
        appearance: Appearance,
        variant_id: &str,
        accent_selection: AccentSelection,
        surface_preference: SurfacePreference,
    ) -> Self {
        let registry = ThemeRegistry::active();
        let fallback_id = match appearance {
            Appearance::Dark => "zeron-dark",
            Appearance::Light => "zeron-light",
        };
        let variant = registry
            .variant(variant_id)
            .filter(|variant| model_appearance(variant.appearance) == appearance)
            .or_else(|| registry.variant(fallback_id))
            .expect("the built-in registry contains both Zeron appearances");
        Self::from_variant(variant, accent_selection, surface_preference)
    }

    pub(crate) fn from_variant(
        variant: &ThemeVariant,
        accent_selection: AccentSelection,
        surface_preference: SurfacePreference,
    ) -> Self {
        let appearance = model_appearance(variant.appearance);
        let accent_color = match accent_selection {
            AccentSelection::ThemeDefault => AccentColor::Zeron,
            AccentSelection::Preset(preset) => preset.into(),
        };
        let mut theme = Self::for_preferences(appearance, accent_color);
        let colors = &variant.colors;
        let accent = variant.accent_for(accent_selection);
        let text_backgrounds = [
            colors.background,
            colors.shell,
            colors.raised,
            colors.card,
            colors.dialog,
            colors.overlay,
            colors.input,
        ];
        let is_curated_builtin = ThemeRegistry::builtin()
            .variant(&variant.id)
            .is_some_and(|builtin| builtin == variant);
        let safe_text = if is_curated_builtin {
            colors.text
        } else {
            harden_model_foreground(colors.text, &text_backgrounds, 4.5, None)
        };
        let safe_text_muted = if is_curated_builtin {
            colors.text_muted
        } else {
            harden_model_foreground(colors.text_muted, &text_backgrounds, 4.5, Some(safe_text))
        };
        theme.variant_id = variant.id.clone().into();
        theme.family_id = variant.family_id.clone().into();
        theme.accent_selection = accent_selection;
        theme.surface_preference = surface_preference;
        theme.surface_treatment = surface_preference.resolve(variant.recommended_surface_treatment);
        theme.bg = model_color(colors.background);
        theme.surface = model_color(colors.shell);
        theme.surface_raised = model_color(colors.raised);
        theme.surface_card = model_color(colors.card);
        theme.surface_dialog = model_color(colors.dialog);
        theme.surface_overlay = model_color(colors.overlay);
        theme.element_hover = model_color(colors.hover);
        theme.element_active = model_color(colors.active);
        theme.border = model_color(colors.border);
        theme.border_strong = model_color(colors.border_strong);
        theme.text = model_color(safe_text);
        theme.text_muted = model_color(safe_text_muted);
        theme.text_faint = model_color(colors.text_faint);
        theme.text_dim = model_color(safe_text_muted);
        theme.solid = model_color(colors.solid);
        theme.on_solid = model_color(if is_curated_builtin {
            colors.on_solid
        } else {
            harden_model_foreground(colors.on_solid, &[colors.solid], 4.5, Some(safe_text))
        });
        theme.accent = model_color(accent.primary);
        theme.accent_strong = model_color(accent.strong);
        theme.accent_wash = model_color(accent.wash);
        theme.on_accent = model_color(accent.on);
        theme.danger = model_color(colors.danger);
        theme.danger_muted = model_color(colors.danger_muted);
        theme.warning = model_color(colors.warning);
        theme.warning_muted = model_color(colors.warning_muted);
        theme.success = model_color(colors.success);
        theme.success_muted = model_color(colors.success_muted);
        theme.busy = model_color(accent.activity);
        theme.glyph = GlyphPalette {
            light: model_color(accent.glyph[0]),
            mid: model_color(accent.glyph[1]),
            deep: model_color(accent.glyph[2]),
        };
        theme.surface_raised_hover = model_color(colors.raised);
        theme.band = model_color(colors.hover);
        theme.input_bg = model_color(colors.input);
        theme.selection = model_color(accent.selection);
        theme.cursor = model_color(colors.cursor);
        theme.caret = model_color(accent.caret);
        theme.danger_strong = model_color(colors.danger);
        theme.code_text = model_color(accent.primary);
        theme.code_wash = model_color(accent.wash);
        theme.syntax = SyntaxPalette::from_variant(variant, theme.syntax);
        theme.diff_add = model_color(colors.diff_add);
        theme.diff_del = model_color(colors.diff_delete);
        theme.diff_hunk_bg = model_color(colors.diff_hunk);
        theme.terminal = TerminalColors::from_variant(variant);
        if !is_curated_builtin {
            theme.terminal.foreground = model_color(harden_model_foreground(
                variant.terminal.foreground,
                &[variant.terminal.background],
                4.5,
                Some(safe_text),
            ));
        }
        theme
    }

    /// Install the theme for `appearance` as the gpui global and point the
    /// context-free paint helpers at it. The **only** way the appearance should
    /// change — setting the global directly leaves [`current_appearance`] stale.
    pub fn install(appearance: Appearance, cx: &mut App) {
        Self::install_preferences(appearance, AccentColor::default(), cx);
    }

    pub fn install_preferences(appearance: Appearance, accent: AccentColor, cx: &mut App) {
        let accent_changed = cx
            .try_global::<Theme>()
            .is_some_and(|theme| theme.accent_color != accent);
        set_current_appearance(appearance);
        let next = Self::for_preferences(appearance, accent)
            .with_font_sans(crate::typography::effective_family_name(cx));
        sync_gpui_base_scrollbar(&next, cx);
        cx.set_global(next);
        // An accent-only swap leaves CURRENT_APPEARANCE unchanged, but cached
        // resolved colors still need to be discarded for the next frame.
        if accent_changed {
            bump_style_generation();
        }
    }

    pub fn install_selection(
        appearance: Appearance,
        variant_id: &str,
        accent_selection: AccentSelection,
        surface_preference: SurfacePreference,
        cx: &mut App,
    ) {
        Self::install_selection_inner(
            appearance,
            variant_id,
            accent_selection,
            surface_preference,
            false,
            cx,
        );
    }

    /// Re-resolve a selection after its registry entry changed in place.
    /// Linked themes keep stable ids across reloads, so identity comparisons
    /// cannot detect that their resolved colors moved. Forcing the generation
    /// invalidates paint caches that contain already-resolved colors.
    pub fn reinstall_selection(
        appearance: Appearance,
        variant_id: &str,
        accent_selection: AccentSelection,
        surface_preference: SurfacePreference,
        cx: &mut App,
    ) {
        Self::install_selection_inner(
            appearance,
            variant_id,
            accent_selection,
            surface_preference,
            true,
            cx,
        );
    }

    fn install_selection_inner(
        appearance: Appearance,
        variant_id: &str,
        accent_selection: AccentSelection,
        surface_preference: SurfacePreference,
        force_generation: bool,
        cx: &mut App,
    ) {
        let next =
            Self::for_selection(appearance, variant_id, accent_selection, surface_preference)
                .with_font_sans(crate::typography::effective_family_name(cx));
        let changed = cx.try_global::<Theme>().is_some_and(|theme| {
            theme.variant_id != next.variant_id
                || theme.accent_selection != next.accent_selection
                || theme.surface_preference != next.surface_preference
                || theme.appearance != next.appearance
        });
        set_current_appearance(appearance);
        sync_gpui_base_scrollbar(&next, cx);
        cx.set_global(next);
        if changed || force_generation {
            bump_style_generation();
        }
    }

    /// Read the theme global.
    pub fn of(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    /// Overlay ink at `alpha` — see [`ink`].
    pub fn ink(&self, alpha: f32) -> Hsla {
        ink_for(self.appearance, alpha)
    }

    /// Hairline ink at `alpha` — see [`hairline`].
    pub fn hairline(&self, alpha: f32) -> Hsla {
        hairline_for(self.appearance, alpha)
    }

    /// State wash at `alpha` — see [`wash`].
    pub fn wash(&self, alpha: f32) -> Hsla {
        wash_for(self.appearance, alpha)
    }
}

fn scrollbar_thumb_colors(theme: &Theme) -> (Hsla, Hsla, Hsla) {
    (
        theme.text.opacity(0.30),
        theme.text.opacity(0.42),
        theme.text.opacity(0.55),
    )
}

fn sync_gpui_base_scrollbar(theme: &Theme, cx: &mut App) {
    let (normal, hover, active) = scrollbar_thumb_colors(theme);
    gpui_base::Theme::global_mut(cx).scrollbar.styles = gpui_base::ScrollbarStyles::default()
        .thumb(|style| style.bg(normal))
        .thumb_hover(|style| style.bg(hover))
        .thumb_active(|style| style.bg(active));
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}

fn system_sans() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

fn system_mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

/// A neutral (chroma 0) oklch tone as Hsla. Chroma 0 means r == g == b exactly,
/// so this goes straight to an achromatic Hsla (skipping the hue math avoids
/// float-noise saturation).
pub fn neutral(lightness: f32) -> Hsla {
    let [v, _, _] = oklch_to_srgb(lightness, 0.0, 0.0);
    hsla(0.0, 0.0, v, 1.0)
}

/// Translucent **fill** ink for interactive states and chip plates: soft-white on
/// dark, soft-black on light at [`INK_FILL_SCALE`] of the alpha.
///
/// Alphas are quoted in *dark-mode terms* at every call site — the dark theme is
/// the tuned one — and the light value is derived. Callers keep one number and
/// both appearances stay in the relationship the dark tuning established.
///
/// Fills must never rest on transparent BLACK in dark mode: fully opaque washes
/// killed the glass and flashed dark mid-fade (user reports), so hover fades rest
/// on `ink(0.0)`, which stays tonally correct at zero alpha.
pub fn ink(alpha: f32) -> Hsla {
    ink_for(current_appearance(), alpha)
}

fn ink_for(appearance: Appearance, alpha: f32) -> Hsla {
    match appearance {
        // Soft-white, not pure white: alphas are high enough to stay visible at
        // the brightest backdrop the 0.90 glass scrim can produce.
        Appearance::Dark => hsla(0.0, 0.0, 1.0, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.0, alpha * INK_FILL_SCALE),
    }
}

/// Translucent **hairline** ink for borders, dividers and rings: white on dark,
/// black on light at [`INK_HAIRLINE_SCALE`] of the alpha.
///
/// Separate from [`ink`] because edges and fills scale in opposite directions
/// when the field brightens — a 1px line needs *more* ink on white, a plate needs
/// less.
pub fn hairline(alpha: f32) -> Hsla {
    hairline_for(current_appearance(), alpha)
}

fn hairline_for(appearance: Appearance, alpha: f32) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 1.0, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.0, (alpha * INK_HAIRLINE_SCALE).min(0.5)),
    }
}

/// Interactive-state wash: a softened [`ink`] that stops short of pure black or
/// white so hover plates read as tinted glass rather than paint.
pub fn wash(alpha: f32) -> Hsla {
    wash_for(current_appearance(), alpha)
}

fn wash_for(appearance: Appearance, alpha: f32) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.92, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.10, alpha * INK_FILL_SCALE),
    }
}

/// Alpha of the standard modal backdrop in dark mode. Call sites that need a
/// heavier or lighter scrim pass their own dark-mode alpha to [`scrim`].
pub const SCRIM_ALPHA_DARK: f32 = 0.60;

/// Modal backdrop at `alpha_dark` (quoted, as everywhere, in dark-mode terms).
///
/// Black in both appearances — a scrim's job is to darken what is behind it, and
/// a "light scrim" of white would wash the modal out rather than seat it. What
/// changes is strength: on a bright field a dark-mode-weight scrim reads as a
/// blackout, so light mode scales to roughly half.
pub fn scrim(alpha_dark: f32) -> Hsla {
    scrim_for(current_appearance(), alpha_dark)
}

fn scrim_for(appearance: Appearance, alpha_dark: f32) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.0, alpha_dark),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.32 * (alpha_dark / SCRIM_ALPHA_DARK)),
    }
}

/// Recessed band behind a palette/picker header or footer strip.
///
/// A free function as well as a [`Theme`] field because the picker chrome that
/// paints it is built from context-free helpers; both resolve to the same value.
pub fn band() -> Hsla {
    band_for(current_appearance())
}

fn band_for(appearance: Appearance) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.0, 0.16),
        // A recessed strip on white needs far less ink than on near-black; the
        // dark 16% would read as a bruise.
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.045),
    }
}

/// Selected-state glass treatment (tabs, session rows, space rows): a
/// TRANSLUCENT wash the vibrancy reads through — heavier flat washes blocked
/// the glass (user request). Dark: the 11% [`wash`]. Light: the tone-flipped
/// wash at 6% — 11% black read too dark over the bright frost (user report;
/// light also previously ran a near-opaque white chip, rejected the same
/// way). Same fill as [`Theme::glass_hover`] — the ring in
/// [`glass_selected_shadows`] is what distinguishes selection. Selection
/// *inside floating cards* is different — see [`card_selected_bg`].
pub fn glass_selected_bg() -> Hsla {
    match current_appearance() {
        Appearance::Dark => wash(0.11),
        Appearance::Light => wash(0.06),
    }
}

/// The user message bubble's plate: the same translucent wash family as
/// [`glass_selected_bg`], one step softer — at the selection weight the
/// bubble read too strong for settled content (user report), and an opaque
/// plate before that read as a solid slab over glass.
pub fn user_bubble_bg() -> Hsla {
    match current_appearance() {
        Appearance::Dark => wash(0.08),
        Appearance::Light => wash(0.04),
    }
}

/// Selected/keyboard-active treatment for rows and chips INSIDE a floating
/// card (menu rows, the picker rail, segmented chips). The card is already the
/// bright plane in light mode, so a white lift can't read there — selection is
/// the tone-flipped grey wash, at 6% (dark's 11% read too dark on the bright
/// plane, user report).
pub fn card_selected_bg() -> Hsla {
    match current_appearance() {
        Appearance::Dark => wash(0.11),
        Appearance::Light => wash(0.06),
    }
}

/// The selected chip's bright outline, as an INSET shadow: gpui paints inset
/// shadows ON TOP of the background, edges only — a border with zero layout
/// cost. Drop shadows are filled rects painted BEHIND the element, and behind
/// a 5% fill they showed straight through as an opaque dark plate with a
/// greyed ring (user report) — nothing may paint behind a glass chip.
///
/// Light pins the ring at a flat 7% black rather than the scaled hairline:
/// heavier rings (the [`INK_HAIRLINE_SCALE`]d value, then 12%) outlined every
/// selected chip in a dark box (user reports) — the ring should define the
/// chip the way dark's 9% white ring does, not frame it.
///
/// There is deliberately NO drop-shadow seat under the light chip. Three
/// recipes were tried (a tight 10% layer, a 6% contact + 5% ambient pair, a
/// lone 4% whisper) and every one failed on sight: layers sum into a grey rim
/// exactly where the chip meets the frost, gpui's small-radius blur reads
/// coarse on a bright field, and the tab strip is a scroll container that
/// clips its children vertically — any shadow escaping the chip gets cut off
/// mid-fade. The near-opaque fill plus the ring carry selection, exactly as
/// dark's wash plus ring does; the two appearances share one recipe now.
pub fn glass_selected_shadows() -> Vec<gpui::BoxShadow> {
    card_selected_shadows()
}

/// Selection outline for rows and chips INSIDE a floating card (menu rows,
/// the picker rail, segmented chips): the inset ring alone, in both
/// appearances. Card rows fill with a translucent wash
/// ([`card_selected_bg`]), and a drop shadow — a filled rect painted BEHIND
/// the element — shows straight through a translucent fill as a grey plate
/// (the same lesson [`glass_selected_shadows`] records for dark glass). The
/// card already carries the elevation shadow; selection inside it only needs
/// the edge.
pub fn card_selected_shadows() -> Vec<gpui::BoxShadow> {
    let color = match current_appearance() {
        Appearance::Dark => hairline(0.09),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.07),
    };
    vec![gpui::BoxShadow {
        color,
        offset: gpui::point(gpui::px(0.0), gpui::px(0.0)),
        blur_radius: gpui::px(0.0),
        spread_radius: gpui::px(1.0),
        inset: true,
    }]
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ `#0d0d0d`)
/// — for surfaces matched against reference-screenshot samples.
pub fn grey(value: u8) -> Hsla {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// Convert an oklch color (CSS notation: L 0..1, C, H in degrees) to gpui Hsla.
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Hsla {
    let [r, g, b] = oklch_to_srgb(l, c, h_deg);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsla(h, s, l, 1.0)
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
/// Reference: Björn Ottosson's OKLab definition (the same matrices CSS Color 4 uses).
pub(crate) fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    // LMS → linear sRGB
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    [gamma_encode(r), gamma_encode(g), gamma_encode(b)]
}

fn gamma_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0..1 components) → HSL, all components 0..1 (gpui's Hsla convention).
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (h, s, l)
}

/// HSL (gpui convention, all 0..1) → sRGB components 0..1.
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    if s <= f32::EPSILON {
        return [l, l, l];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0)]
}

/// WCAG 2.1 relative luminance of an opaque color.
pub fn relative_luminance(color: Hsla) -> f32 {
    let lin = |c: f32| {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let [r, g, b] = hsl_to_rgb(color.h, color.s, color.l);
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG 2.1 contrast ratio between two opaque colors (1.0 … 21.0).
///
/// Used by the palette tests to prove each light token reproduces the contrast
/// its dark counterpart had, rather than merely looking plausible.
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn painted_contrast(foreground: Hsla, background: Hsla) -> f32 {
    contrast_ratio(flatten(foreground, background), background)
}

/// Composite `fg` (which may be translucent) over an opaque `bg`, returning the
/// opaque result — the color the eye actually receives.
pub fn flatten(fg: Hsla, bg: Hsla) -> Hsla {
    let a = fg.a.clamp(0.0, 1.0);
    let [fr, fg_, fb] = hsl_to_rgb(fg.h, fg.s, fg.l);
    let [br, bg_, bb] = hsl_to_rgb(bg.h, bg.s, bg.l);
    let (h, s, l) = rgb_to_hsl(
        fr * a + br * (1.0 - a),
        fg_ * a + bg_ * (1.0 - a),
        fb * a + bb * (1.0 - a),
    );
    hsla(h, s, l, 1.0)
}

/// Linear per-component mix of two colors (paint helper for the gradient spinner).
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    // Mix through hue naively — both spinner endpoints sit close enough on the
    // wheel that shortest-arc handling isn't needed for our palette.
    hsla(
        lerp(a.h, b.h),
        lerp(a.s, b.s),
        lerp(a.l, b.l),
        lerp(a.a, b.a),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_u8(c: [f32; 3]) -> [u8; 3] {
        [
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        ]
    }

    #[test]
    fn neutral_950_is_0a0a0a() {
        // oklch(0.145 0 0) is Tailwind neutral-950, zeron's app background.
        let rgb = srgb_u8(oklch_to_srgb(0.145, 0.0, 0.0));
        assert_eq!(rgb, [10, 10, 10]);
    }

    #[test]
    fn scrollbar_thumbs_follow_the_active_appearance() {
        let dark = scrollbar_thumb_colors(&Theme::dark());
        let light = scrollbar_thumb_colors(&Theme::light());

        assert!(dark.0.l > Theme::dark().bg.l);
        assert!(light.0.l < Theme::light().bg.l);
        assert_eq!([dark.0.a, dark.1.a, dark.2.a], [0.30, 0.42, 0.55]);
        assert_eq!([light.0.a, light.1.a, light.2.a], [0.30, 0.42, 0.55]);
    }

    #[test]
    fn oklch_accents_match_reference() {
        // Reference values computed independently (CSS Color 4 matrices).
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.673, 0.182, 276.935)),
            [124, 134, 255]
        ); // indigo-400
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.704, 0.191, 22.216)),
            [255, 100, 103]
        ); // red-400
        assert_eq!(srgb_u8(oklch_to_srgb(0.828, 0.189, 84.429)), [255, 185, 0]); // amber-400
    }

    #[test]
    fn hsl_roundtrips_through_rgb() {
        for c in [
            Theme::dark().accent,
            Theme::dark().warning,
            Theme::light().accent,
            Theme::light().danger,
            neutral(0.556),
        ] {
            let [r, g, b] = hsl_to_rgb(c.h, c.s, c.l);
            let (h, s, l) = rgb_to_hsl(r, g, b);
            assert!((l - c.l).abs() < 1e-3, "lightness drift for {c:?}");
            assert!((s - c.s).abs() < 1e-3, "saturation drift for {c:?}");
            if c.s > 1e-3 {
                assert!((h - c.h).abs() < 1e-3, "hue drift for {c:?}");
            }
        }
    }

    #[test]
    fn contrast_ratio_hits_known_anchors() {
        let white = grey(0xff);
        let black = grey(0x00);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
        // Symmetric regardless of argument order.
        assert!((contrast_ratio(black, white) - contrast_ratio(white, black)).abs() < 1e-4);
    }

    #[test]
    fn zeron_accent_is_the_exact_upstream_default() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_eq!(dark.accent_color, AccentColor::Zeron);
        assert_eq!(dark.accent, oklch(0.673, 0.182, 276.935));
        assert_eq!(dark.accent_strong, oklch(0.585, 0.233, 277.117));
        assert_eq!(dark.code_text, dark.accent);
        assert_eq!(dark.busy, dark.accent);
        assert_eq!(dark.glyph.mid, dark.accent);
        assert_eq!(dark.caret, dark.accent);
        assert_eq!(light.accent, oklch(0.511, 0.262, 276.966));
        assert_eq!(light.accent_strong, oklch(0.511, 0.262, 276.966));
        assert_eq!(light.code_text, light.accent);
        assert_eq!(light.busy, light.accent);
        assert_eq!(light.glyph.mid, light.accent);
        assert_eq!(light.caret, light.accent);
    }

    #[test]
    fn previous_preview_accent_names_migrate_without_resetting_settings() {
        for old_default in ["violet", "indigo", "red", "purple"] {
            assert_eq!(
                serde_json::from_str::<AccentColor>(&format!(r#""{old_default}""#)).unwrap(),
                AccentColor::Zeron
            );
        }
        assert_eq!(
            serde_json::from_str::<AccentColor>(r#""teal""#).unwrap(),
            AccentColor::Cyan
        );
    }

    #[test]
    fn theme_recommendation_and_surface_override_resolve_independently() {
        let catppuccin = Theme::for_selection(
            Appearance::Dark,
            "catppuccin-mocha",
            AccentSelection::ThemeDefault,
            SurfacePreference::ThemeDefault,
        );
        let zeron = Theme::dark();
        assert_ne!(catppuccin.surface, zeron.surface);
        assert_eq!(catppuccin.busy, catppuccin.accent);
        assert_eq!(catppuccin.glyph.mid, catppuccin.accent);
        assert_eq!(catppuccin.surface_treatment, SurfaceTreatment::Opaque);
        assert!(!catppuccin.is_glass());

        let frosted = Theme::for_selection(
            Appearance::Dark,
            "catppuccin-mocha",
            AccentSelection::ThemeDefault,
            SurfacePreference::Frosted,
        );
        assert_eq!(frosted.variant_id, catppuccin.variant_id);
        assert_eq!(frosted.accent, catppuccin.accent);
        assert_eq!(frosted.surface_treatment, SurfaceTreatment::Frosted);
        assert_eq!(frosted.glass().h, frosted.surface.h);
        assert_eq!(frosted.glass().s, frosted.surface.s);
        assert_eq!(frosted.glass().l, frosted.surface.l);

        let opaque_zeron = Theme::for_selection(
            Appearance::Dark,
            "zeron-dark",
            AccentSelection::ThemeDefault,
            SurfacePreference::Opaque,
        );
        assert_eq!(opaque_zeron.surface_treatment, SurfaceTreatment::Opaque);
        assert_eq!(opaque_zeron.glass(), opaque_zeron.surface);
    }

    #[test]
    fn forced_frost_preserves_shell_text_contrast_for_every_builtin() {
        let registry = ThemeRegistry::builtin();
        for variant in registry.families.iter().flat_map(|family| &family.variants) {
            let appearance = model_appearance(variant.appearance);
            let theme = Theme::from_variant(
                variant,
                AccentSelection::ThemeDefault,
                SurfacePreference::Frosted,
            );
            let adverse_backdrop = match appearance {
                Appearance::Dark => grey(0xff),
                Appearance::Light => grey(0),
            };
            let composite = flatten(theme.glass(), adverse_backdrop);
            assert!(
                painted_contrast(theme.text, composite) >= 4.5,
                "{} primary shell text",
                variant.id
            );
            assert!(
                painted_contrast(theme.text_muted, composite) >= 3.0,
                "{} muted shell text",
                variant.id
            );
            for (surface_name, surface) in [
                (
                    "floating overlay",
                    flatten(theme.glass_overlay(), adverse_backdrop),
                ),
                ("settings card", flatten(theme.card_glass_bg(), composite)),
                ("input", flatten(theme.input_glass_bg(), composite)),
            ] {
                assert!(
                    painted_contrast(theme.text, surface) >= 4.5,
                    "{} primary text on {surface_name}",
                    variant.id
                );
                assert!(
                    painted_contrast(theme.text_muted, surface) >= 3.0,
                    "{} muted text on {surface_name}",
                    variant.id
                );
            }
        }
    }

    #[test]
    fn runtime_hardening_protects_native_custom_theme_edits() {
        let mut variant = ThemeRegistry::builtin()
            .variant("zeron-dark")
            .unwrap()
            .clone();
        variant.colors.text = variant.colors.background;
        variant.colors.text_muted = variant.colors.background;
        variant.colors.on_solid = variant.colors.solid;
        variant.terminal.foreground = variant.terminal.background;

        let theme = Theme::from_variant(
            &variant,
            AccentSelection::ThemeDefault,
            SurfacePreference::Opaque,
        );
        assert!(painted_contrast(theme.text, theme.bg) >= 4.5);
        assert!(painted_contrast(theme.text_muted, theme.bg) >= 4.5);
        assert!(painted_contrast(theme.on_solid, theme.solid) >= 4.5);
        assert!(painted_contrast(theme.terminal.foreground, theme.terminal.background) >= 4.5);
    }

    #[test]
    fn runtime_hardening_leaves_curated_builtin_text_unchanged() {
        for variant in ThemeRegistry::builtin()
            .families
            .iter()
            .flat_map(|family| &family.variants)
        {
            let theme = Theme::from_variant(
                variant,
                AccentSelection::ThemeDefault,
                SurfacePreference::Opaque,
            );
            assert_eq!(
                theme.text,
                model_color(variant.colors.text),
                "{} text",
                variant.id
            );
            assert_eq!(
                theme.text_muted,
                model_color(variant.colors.text_muted),
                "{} muted text",
                variant.id
            );
        }
    }

    #[test]
    fn every_accent_is_one_coherent_color_identity() {
        let default_dark = Theme::dark();
        let default_light = Theme::light();
        for accent in AccentColor::ALL {
            for (theme, baseline) in [
                (Theme::dark_with_accent(accent), &default_dark),
                (Theme::light_with_accent(accent), &default_light),
            ] {
                assert_eq!(theme.accent_color, accent);
                for (surface_name, surface) in [
                    ("content", theme.bg),
                    ("shell", theme.surface),
                    ("card", theme.surface_card),
                ] {
                    assert!(
                        contrast_ratio(theme.accent, surface) >= 4.5,
                        "{} {:?} accent is only {:.2}:1 on the {surface_name} surface",
                        accent.label(),
                        theme.appearance,
                        contrast_ratio(theme.accent, surface)
                    );
                }
                assert!(
                    contrast_ratio(theme.on_accent, theme.accent_strong) >= 4.0,
                    "{} {:?} solid is only {:.2}:1 against its label",
                    accent.label(),
                    theme.appearance,
                    contrast_ratio(theme.on_accent, theme.accent_strong)
                );
                assert!(contrast_ratio(theme.code_text, theme.bg) >= 4.5);
                assert!(contrast_ratio(theme.busy, theme.bg) >= 3.0);
                assert_eq!(theme.code_text, theme.accent);
                assert_eq!(theme.busy, theme.accent);
                assert_eq!(theme.glyph.mid, theme.accent);
                assert_ne!(theme.glyph.light, theme.glyph.mid);
                assert_ne!(theme.glyph.deep, theme.glyph.mid);
                assert_eq!(theme.caret, theme.accent);
                assert_eq!(
                    theme.selection,
                    theme.accent.opacity(if theme.appearance.is_dark() {
                        0.35
                    } else {
                        0.24
                    })
                );
                assert!(hue_distance(theme.accent.h, theme.accent_strong.h) <= 0.04);

                // Semantic colors and code syntax keep their meaning. Accent
                // preferences only recolor interactive identity roles.
                assert_eq!(theme.danger, baseline.danger);
                assert_eq!(theme.warning, baseline.warning);
                assert_eq!(theme.success, baseline.success);
                assert_eq!(theme.diff_add, baseline.diff_add);
                assert_eq!(theme.diff_del, baseline.diff_del);
                if accent != AccentColor::Zeron {
                    assert_ne!(theme.code_text, baseline.code_text);
                    assert_ne!(theme.busy, baseline.busy);
                    assert_ne!(theme.selection, baseline.selection);
                }
                assert_eq!(theme.syntax.keyword, baseline.syntax.keyword);
                assert_eq!(theme.syntax.string, baseline.syntax.string);
                assert_eq!(theme.syntax.number, baseline.syntax.number);
            }
        }
    }

    fn hue_distance(a: f32, b: f32) -> f32 {
        let distance = (a - b).abs();
        distance.min(1.0 - distance)
    }

    #[test]
    fn every_settings_swatch_has_a_distinct_primary() {
        let previews = AccentColor::ALL.map(|accent| {
            let theme = Theme::dark_with_accent(accent);
            (accent, theme.accent)
        });
        for (index, (accent, color)) in previews.iter().enumerate() {
            for (other_accent, other_color) in previews.iter().skip(index + 1) {
                assert_ne!(
                    color,
                    other_color,
                    "{} and {} share the same primary",
                    accent.label(),
                    other_accent.label()
                );
            }
        }
    }

    /// The core claim of the light palette: it is *paired* to dark by contrast
    /// ratio, not mirrored by lightness. Each text token must land within 1.0 of
    /// its counterpart's ratio against its own background.
    #[test]
    fn text_contrast_is_paired_across_appearances() {
        let (d, l) = (Theme::dark(), Theme::light());
        for (name, dark_fg, light_fg) in [
            ("text", d.text, l.text),
            ("text_muted", d.text_muted, l.text_muted),
            ("text_faint", d.text_faint, l.text_faint),
        ] {
            let dr = contrast_ratio(dark_fg, d.bg);
            let lr = contrast_ratio(light_fg, l.bg);
            assert!(
                (dr - lr).abs() < 1.0,
                "{name}: dark {dr:.2}:1 vs light {lr:.2}:1 — not a matched pair"
            );
        }
    }

    /// Body and secondary text must clear WCAG AA (4.5:1) against **both** planes
    /// they can land on, in both appearances.
    ///
    /// `text_faint` is held to a lower floor on purpose. It is placeholder and
    /// disabled-control copy only, which WCAG 1.4.3 exempts, and the *existing
    /// dark palette* already measures ~4.2:1 there (neutral-500 on #060606). The
    /// light tone is matched to that inherited number rather than raised past it,
    /// so the two appearances stay siblings; raising the floor is a palette
    /// decision for both modes at once, not something light mode should do alone.
    #[test]
    fn text_tones_clear_wcag_aa() {
        for t in [Theme::dark(), Theme::light()] {
            for (name, fg, floor) in [
                ("text", t.text, 4.5),
                ("text_muted", t.text_muted, 4.5),
                ("text_dim", t.text_dim, 4.5),
                ("text_faint", t.text_faint, 4.1),
            ] {
                let on_bg = contrast_ratio(fg, t.bg);
                let on_surface = contrast_ratio(fg, t.surface);
                assert!(
                    on_bg >= floor,
                    "{:?} {name} on bg is {on_bg:.2}:1, below {floor}",
                    t.appearance
                );
                assert!(
                    on_surface >= floor,
                    "{:?} {name} on surface is {on_surface:.2}:1, below {floor}",
                    t.appearance
                );
            }
        }
    }

    /// Accents are the tokens a naive invert gets most wrong: the dark theme's
    /// 400-step indigo/red land near 3:1 on white. The light palette drops to the
    /// 600 step at the same hue, which must clear AA for non-text UI (3:1) and,
    /// for the accent proper, body-text AA.
    #[test]
    fn accents_clear_contrast_on_their_background() {
        let l = Theme::light();
        assert!(
            contrast_ratio(l.accent, l.bg) >= 4.5,
            "light accent {:.2}:1",
            contrast_ratio(l.accent, l.bg)
        );
        assert!(
            contrast_ratio(l.danger, l.bg) >= 4.0,
            "light danger {:.2}:1",
            contrast_ratio(l.danger, l.bg)
        );
        for c in [l.warning, l.success, l.busy] {
            assert!(
                contrast_ratio(c, l.bg) >= 3.0,
                "light status color {:.2}:1 — below the 3:1 non-text floor",
                contrast_ratio(c, l.bg)
            );
        }
        // And the dark 400-step accents would NOT have cleared it — this is why
        // the light theme reassigns rather than reuses.
        let d = Theme::dark();
        assert!(
            contrast_ratio(d.warning, l.bg) < 3.0,
            "dark amber-400 unexpectedly passes on white; the invert-is-wrong \
             premise needs rechecking"
        );
    }

    /// Code is *text*, so syntax tones are held to the body-copy bar, not the
    /// 3:1 non-text floor. These are the tokens most likely to be picked by eye
    /// from a dark-theme screenshot and silently fail once the page turns white.
    #[test]
    fn code_and_syntax_tones_are_readable() {
        for t in [Theme::dark(), Theme::light()] {
            for (name, fg) in [
                ("code_text", t.code_text),
                ("syntax_keyword", t.syntax.keyword),
                ("syntax_string", t.syntax.string),
                ("syntax_number", t.syntax.number),
            ] {
                let r = contrast_ratio(fg, t.bg);
                assert!(r >= 4.5, "{:?} {name} is {r:.2}:1 on bg", t.appearance);
            }
            // Diff tints mark whole rows; the 3:1 non-text floor applies.
            for (name, fg) in [("diff_add", t.diff_add), ("diff_del", t.diff_del)] {
                let r = contrast_ratio(fg, t.bg);
                assert!(r >= 3.0, "{:?} {name} is {r:.2}:1 on bg", t.appearance);
            }
        }
    }

    #[test]
    fn syntax_palette_resolves_every_kind_on_code_and_diff_backgrounds() {
        let kinds = [
            HighlightKind::Comment,
            HighlightKind::Keyword,
            HighlightKind::String,
            HighlightKind::StringSpecial,
            HighlightKind::Escape,
            HighlightKind::Number,
            HighlightKind::Boolean,
            HighlightKind::Type,
            HighlightKind::TypeBuiltin,
            HighlightKind::Constructor,
            HighlightKind::Function,
            HighlightKind::FunctionBuiltin,
            HighlightKind::Macro,
            HighlightKind::Property,
            HighlightKind::Constant,
            HighlightKind::Variable,
            HighlightKind::VariableSpecial,
            HighlightKind::Parameter,
            HighlightKind::Operator,
            HighlightKind::Punctuation,
            HighlightKind::Tag,
            HighlightKind::Attribute,
            HighlightKind::Label,
            HighlightKind::MarkupHeading,
            HighlightKind::MarkupRaw,
            HighlightKind::MarkupLink,
            HighlightKind::MarkupReference,
            HighlightKind::MarkupEmphasis,
            HighlightKind::MarkupStrong,
            HighlightKind::Embedded,
            HighlightKind::Invalid,
        ];
        for accent in AccentColor::ALL {
            for theme in [
                Theme::dark_with_accent(accent),
                Theme::light_with_accent(accent),
            ] {
                let add_bg = flatten(theme.diff_add.opacity(0.055), theme.bg);
                let del_bg = flatten(theme.diff_del.opacity(0.055), theme.bg);
                for kind in kinds {
                    let color = theme.syntax.color(kind);
                    let floor = if matches!(
                        kind,
                        HighlightKind::Comment
                            | HighlightKind::Operator
                            | HighlightKind::Punctuation
                            | HighlightKind::Embedded
                    ) {
                        3.0
                    } else {
                        4.5
                    };
                    for (name, background) in [("code", theme.bg), ("add", add_bg), ("del", del_bg)]
                    {
                        let ratio = contrast_ratio(color, background);
                        assert!(
                            ratio >= floor,
                            "{} {:?} {kind:?} is {ratio:.2}:1 on {name}",
                            accent.label(),
                            theme.appearance
                        );
                    }
                }
            }
        }
    }

    /// The caret is a 2px bar, so the 3:1 non-text floor applies — but it is the
    /// one element the user is actively hunting for, and the dark-mode blue is
    /// far too light to survive on white unchanged.
    #[test]
    fn caret_is_findable_on_its_background() {
        for t in [Theme::dark(), Theme::light()] {
            let r = contrast_ratio(t.caret, t.bg);
            assert!(r >= 3.0, "{:?} caret is {r:.2}:1 on bg", t.appearance);
        }
    }

    /// Solid (primary button) plates must carry their label at AA in both modes.
    ///
    /// The accent plate is held to 4.0 rather than 4.5: dark mode's indigo-500
    /// fill — inherited unchanged from the original palette — measures 4.38:1
    /// under white, which clears WCAG AA for the medium-weight 14px labels these
    /// buttons use (large-text AA is 3:1) but not body copy. Light mode's
    /// indigo-600 clears the stricter bar with room to spare.
    #[test]
    fn solid_button_is_legible_in_both_appearances() {
        for t in [Theme::dark(), Theme::light()] {
            let r = contrast_ratio(t.on_solid, t.solid);
            assert!(r >= 7.0, "{:?} solid button {r:.2}:1", t.appearance);
            let a = contrast_ratio(t.on_accent, t.accent_strong);
            assert!(a >= 4.0, "{:?} accent button {a:.2}:1", t.appearance);
        }
    }

    /// Surfaces must stay *distinguishable*, but the direction differs: dark
    /// stacks upward in lightness, light puts the content plane on top and lets
    /// chrome recede. Asserting separation (not a fixed order) is the point.
    #[test]
    fn surfaces_are_separated_in_both_appearances() {
        let d = Theme::dark();
        assert!(d.bg.l < d.surface.l, "dark: chrome sits above content");
        assert!(d.surface.l < d.surface_raised.l, "dark: raised is lighter");

        let l = Theme::light();
        assert!(
            l.surface.l < l.bg.l,
            "light: chrome recedes *below* the content plane"
        );
        assert!(
            (l.bg.l - l.surface.l) > 0.015,
            "light: sidebar must be visibly separated from the panel"
        );
        // Raised surfaces are white in light mode; separation comes from the
        // border, so the border must be strong enough to carry it alone.
        assert!(contrast_ratio(flatten(l.border, l.bg), l.bg) > 1.15);
    }

    /// The dark elevation steps are small but deliberate, and each plane must
    /// stay strictly above the one below. This test exists because collapsing the
    /// ladder onto a single `surface_raised` is the tempting simplification — and
    /// it visibly lifts every popover off its plane.
    #[test]
    fn dark_elevation_ladder_is_strictly_ordered() {
        let d = Theme::dark();
        let ladder = [
            ("bg", d.bg),
            ("surface_card", d.surface_card),
            ("surface_dialog", d.surface_dialog),
            ("surface_overlay", d.surface_overlay),
            ("surface_raised", d.surface_raised),
        ];
        for pair in ladder.windows(2) {
            let ((lower, lo), (upper, hi)) = (pair[0], pair[1]);
            assert!(
                lo.l < hi.l,
                "dark: {upper} ({:.4}) must sit above {lower} ({:.4})",
                hi.l,
                lo.l
            );
        }
    }

    /// Light mode flattens the ladder onto white on purpose — separation comes
    /// from border and shadow. Assert that explicitly so nobody "fixes" it by
    /// reintroducing lightness steps that would tint popovers grey.
    #[test]
    fn light_elevation_is_flat_white_and_leans_on_borders() {
        let l = Theme::light();
        for (name, c) in [
            ("surface_card", l.surface_card),
            ("surface_dialog", l.surface_dialog),
            ("surface_overlay", l.surface_overlay),
        ] {
            assert_eq!(c.l, 1.0, "light {name} should be white");
        }
        // With no lightness step available, the border is the only separator —
        // it has to actually register against the plane behind it.
        assert!(contrast_ratio(flatten(l.border, l.bg), l.bg) > 1.15);
    }

    /// `surface_raised` is the *bare plate* tone — user message bubbles, the
    /// jump-to-bottom pill. Unlike the popover ladder it gets no border and no
    /// shadow, so lightness is the only thing separating it from the panel. It
    /// was white in light mode once, which made the user's own messages
    /// indistinguishable from the page.
    #[test]
    fn bare_plates_are_visible_against_their_panel() {
        for t in [Theme::dark(), Theme::light()] {
            let delta = (t.surface_raised.l - t.bg.l).abs();
            assert!(
                delta > 0.03,
                "{:?} surface_raised ({:.3}) is only {delta:.3} from bg ({:.3}) — \
                 a plate with no border needs lightness to read",
                t.appearance,
                t.surface_raised.l,
                t.bg.l
            );
            // And hovering it has to go somewhere visible too.
            let hover_delta = (t.surface_raised_hover.l - t.surface_raised.l).abs();
            assert!(
                hover_delta > 0.02,
                "{:?} raised-plate hover moves only {hover_delta:.3}",
                t.appearance
            );
        }
    }

    /// Monochrome discipline: neutrals carry no saturation in either appearance.
    #[test]
    fn neutrals_are_achromatic() {
        for t in [Theme::dark(), Theme::light()] {
            for c in [
                t.bg,
                t.surface,
                t.surface_raised,
                t.text,
                t.text_muted,
                t.text_faint,
                t.solid,
                t.on_solid,
            ] {
                assert_eq!(c.s, 0.0, "{:?} neutral has chroma", t.appearance);
                assert_eq!(c.a, 1.0, "{:?} neutral is translucent", t.appearance);
            }
        }
    }

    #[test]
    fn hairlines_and_washes_flip_tone_with_appearance() {
        let _guard = lock_appearance();
        set_current_appearance(Appearance::Dark);
        assert_eq!(hairline(0.1).l, 1.0, "dark hairlines are white");
        assert_eq!(ink(0.1).l, 1.0, "dark fills are white");
        assert_eq!(ink(0.1).a, 0.1, "dark alphas pass through untouched");
        assert_eq!(wash(0.14).l, 0.92, "dark washes are soft-white");

        set_current_appearance(Appearance::Light);
        assert_eq!(hairline(0.1).l, 0.0, "light hairlines are black");
        assert_eq!(ink(0.1).l, 0.0, "light fills are black");
        assert_eq!(wash(0.14).l, 0.10, "light washes are soft-black");
        // Fills keep their alpha; only hairlines are scaled.
        assert_eq!(ink(0.10).a, 0.10, "light fills keep their alpha");
        assert!(hairline(0.10).a > 0.10, "light hairlines strengthen");
        assert!(hairline(0.60).a <= 0.5, "hairline alpha is capped");

        set_current_appearance(Appearance::Dark);
    }

    /// A hover wash has to actually be *visible* against the surface it lands on,
    /// in both appearances — the failure mode of a halved light alpha.
    #[test]
    fn hover_wash_is_visible_on_its_surface() {
        let _guard = lock_appearance();
        for (appearance, theme) in [
            (Appearance::Dark, Theme::dark()),
            (Appearance::Light, Theme::light()),
        ] {
            set_current_appearance(appearance);
            let hovered = flatten(wash(0.14), theme.surface);
            let delta = (hovered.l - theme.surface.l).abs();
            assert!(
                delta > 0.02,
                "{appearance:?} hover wash shifts lightness by only {delta:.4}"
            );
        }
        set_current_appearance(Appearance::Dark);
    }

    /// The regression that shipped: subtle fills are quoted at very low alphas
    /// (`ink(0.03)` is the composer plate, `ink(0.05)` a key cap), and scaling
    /// those down for light mode erased them — the composer rendered as bare text
    /// on white. Assert the faintest fill we actually use still moves the surface
    /// it lands on, in *both* appearances.
    #[test]
    fn faintest_fills_survive_in_both_appearances() {
        let _guard = lock_appearance();
        for (appearance, theme) in [
            (Appearance::Dark, Theme::dark()),
            (Appearance::Light, Theme::light()),
        ] {
            set_current_appearance(appearance);
            for alpha in [0.03, 0.05] {
                let plate = flatten(ink(alpha), theme.bg);
                let delta = (plate.l - theme.bg.l).abs();
                assert!(
                    delta >= 0.02,
                    "{appearance:?} ink({alpha}) shifts its background by only \
                     {delta:.4} — the fill is invisible"
                );
            }
        }
        set_current_appearance(Appearance::Dark);
    }

    /// Both appearances are glass-forward on macOS. Light frost runs heavier
    /// than dark's (a light tint controls the blur less), and floating cards
    /// step their tint coverage up in light so menu text stays on a
    /// known-enough background — assert both relationships so the frost and
    /// the overlay can't drift apart.
    #[test]
    fn both_appearances_stay_frosted_and_light_runs_heavier() {
        if Theme::GLASS_ALPHA < 1.0 {
            let (dark, light) = (Theme::dark(), Theme::light());
            assert!(dark.glass().a < 1.0, "dark keeps its translucent frost");
            assert!(light.glass().a < 1.0, "light is glass-forward like dark");
            assert!(
                light.glass().a > dark.glass().a - f32::EPSILON,
                "a light tint dominates the blur less, so it must not run looser than dark"
            );
            assert!(
                light.glass_overlay().a > dark.glass_overlay().a,
                "light floating cards need more coverage over blur for legible rows"
            );
        } else {
            assert_eq!(Theme::light().glass().a, 1.0);
            assert_eq!(Theme::dark().glass().a, 1.0);
        }
    }

    /// An input plate has to read as *lifted* in both appearances. Dark does that
    /// with a faint white wash; the literal light translation is a faint black
    /// wash, which reads as a dent instead — so light lifts with white plus its
    /// border. Assert the plate is never darker than the panel it sits on.
    #[test]
    fn input_plate_never_reads_as_recessed() {
        for t in [Theme::dark(), Theme::light()] {
            let plate = flatten(t.input_bg, t.bg);
            assert!(
                plate.l >= t.bg.l,
                "{:?} input plate ({:.3}) is darker than its panel ({:.3}) — \
                 that reads as recessed, not raised",
                t.appearance,
                plate.l,
                t.bg.l
            );
        }
    }

    /// Card rows fill with translucent washes, and a drop shadow behind a
    /// translucent fill shows through as a grey plate — selection inside a
    /// floating card must be edge-only. This regressed once: light menu rows
    /// borrowed the glass-chip recipe, drop shadow included.
    #[test]
    fn card_selection_paints_nothing_behind_its_row() {
        let _guard = lock_appearance();
        for appearance in [Appearance::Dark, Appearance::Light] {
            set_current_appearance(appearance);
            for shadow in card_selected_shadows() {
                assert!(
                    shadow.inset,
                    "{appearance:?}: card selection may only paint inset edges"
                );
            }
        }
        set_current_appearance(Appearance::Dark);
    }

    /// Glass selection is edge-only in BOTH appearances — no drop-shadow seat.
    /// Every light seat tried (10% tight, 6%+5% pair, lone 4%) read as a grey
    /// rim or a coarse smudge, and the tab strip clips escaping shadows
    /// vertically (user reports). The ring must also stay subtle enough to
    /// define the chip rather than frame it.
    #[test]
    fn glass_selection_is_edge_only_and_subtle() {
        let _guard = lock_appearance();
        for appearance in [Appearance::Dark, Appearance::Light] {
            set_current_appearance(appearance);
            let shadows = glass_selected_shadows();
            assert!(
                shadows.iter().all(|s| s.inset),
                "{appearance:?}: glass selection may only paint inset edges"
            );
            let ring = shadows.iter().find(|s| s.inset).expect("selection ring");
            assert!(
                ring.color.a <= 0.09,
                "{appearance:?}: ring at {:.2} alpha frames the chip instead of defining it",
                ring.color.a
            );
        }
        set_current_appearance(Appearance::Dark);
    }

    #[test]
    fn appearance_mirror_tracks_installed_theme() {
        let _guard = lock_appearance();
        set_current_appearance(Appearance::Light);
        assert_eq!(current_appearance(), Appearance::Light);
        set_current_appearance(Appearance::Dark);
        assert_eq!(current_appearance(), Appearance::Dark);
    }

    #[test]
    fn appearance_rebuild_preserves_selected_ui_family() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance).with_font_sans("Inter".into());
            assert_eq!(theme.font_sans.as_ref(), "Inter");
            assert_eq!(theme.font_sans_fixed.as_ref(), "Geist");
            assert_eq!(theme.font_mono.as_ref(), "Geist Mono");
        }
    }

    #[test]
    fn window_appearance_maps_onto_ours() {
        use gpui::WindowAppearance as W;
        assert_eq!(Appearance::from_window(W::Light), Appearance::Light);
        assert_eq!(Appearance::from_window(W::VibrantLight), Appearance::Light);
        assert_eq!(Appearance::from_window(W::Dark), Appearance::Dark);
        assert_eq!(Appearance::from_window(W::VibrantDark), Appearance::Dark);
    }

    #[test]
    fn scrim_is_black_but_lighter_in_light_mode() {
        let (d, l) = (Theme::dark(), Theme::light());
        assert_eq!(d.scrim().l, 0.0);
        assert_eq!(l.scrim().l, 0.0);
        assert!(l.scrim().a < d.scrim().a);
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = hsla(0.0, 0.0, 0.0, 1.0);
        let b = hsla(0.5, 1.0, 1.0, 0.0);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        let mid = mix(a, b, 0.5);
        assert!((mid.l - 0.5).abs() < 1e-6 && (mid.a - 0.5).abs() < 1e-6);
        // Out-of-range t clamps.
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn layout_numbers_match_zeron() {
        assert_eq!(Theme::HEADER_HEIGHT, 44.0); // h-11
        assert_eq!(Theme::STATUS_STRIP_HEIGHT, 24.0); // h-6
        assert_eq!(Theme::BUBBLE_RADIUS, 16.0);
    }
}
