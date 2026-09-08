//! Light/dark switching: what the user asked for, what the OS reports, and the
//! plumbing that turns a change in either into a repaint.
//!
//! Three pieces, following the pattern zed uses (`crates/theme/src/theme.rs`
//! `SystemAppearance` + `reload_theme` + `cx.refresh_windows`):
//!
//! 1. [`AppearanceMode`] — the persisted user choice: follow the OS, or pin one.
//! 2. [`AppearanceState`] — a gpui global holding that choice alongside the last
//!    appearance the OS reported, so [`resolve`] can combine them.
//! 3. [`observe_window`] — subscribes to the platform's appearance notification
//!    (macOS `viewDidChangeEffectiveAppearance`) and re-applies.
//!
//! # Why `refresh_windows` and not `notify`
//!
//! Colors are read *imperatively* (`Theme::of(cx).text`) at paint time, not
//! through a reactive binding, so no view knows its colors went stale — a
//! `notify()` on some entity would repaint that entity and nothing else.
//! [`App::refresh_windows`] marks every window dirty *and* disables gpui's
//! per-view prepaint cache for the frame, which is the only thing that forces
//! already-laid-out elements to re-run their paint with the new palette.

use gpui::{App, Global, Subscription, Window};
use serde::{Deserialize, Serialize};
use zeron_theme::{AccentSelection, SurfacePreference, ThemeSelection};

use crate::settings::{self, SavePolicy};
use crate::theme::{Appearance, Theme};

/// The user's appearance preference. Persisted in `ui-settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppearanceMode {
    /// Follow the OS. The default — matches every other native app on the
    /// machine, including when the user has macOS set to switch at sunset.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// Menu/label text.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];
}

/// Global state behind the current theme: what the user chose, and what the OS
/// last said. Kept separate from [`Theme`] itself so that flipping the OS
/// appearance while the user has pinned Light still records the new system value
/// (and takes effect the moment they switch back to `System`).
pub struct AppearanceState {
    pub mode: AppearanceMode,
    pub system: Appearance,
    pub themes: ThemeSelection,
    pub accent: AccentSelection,
    pub surface: SurfacePreference,
}

impl Global for AppearanceState {}

/// Combine the user's choice with the OS state.
pub fn resolve(mode: AppearanceMode, system: Appearance) -> Appearance {
    match mode {
        AppearanceMode::System => system,
        AppearanceMode::Light => Appearance::Light,
        AppearanceMode::Dark => Appearance::Dark,
    }
}

/// Install the appearance globals and the matching theme. Call once at boot,
/// before any window opens, so the first frame is already the right palette
/// (installing later produces a visible dark-to-light flash).
pub fn init(
    mode: AppearanceMode,
    themes: ThemeSelection,
    accent: AccentSelection,
    surface: SurfacePreference,
    cx: &mut App,
) {
    let system = Appearance::from_window(cx.window_appearance());
    tracing::debug!(?mode, ?system, "appearance: initial");
    cx.set_global(AppearanceState {
        mode,
        system,
        themes: themes.clone(),
        accent,
        surface,
    });
    sync_ns_appearance(mode);
    let appearance = resolve(mode, system);
    Theme::install_selection(
        appearance,
        themes.variant_id(model_appearance(appearance)),
        accent,
        surface,
        cx,
    );
}

/// The mode currently in effect (defaults to `System` before [`init`]).
pub fn mode(cx: &App) -> AppearanceMode {
    cx.try_global::<AppearanceState>()
        .map(|s| s.mode)
        .unwrap_or_default()
}

pub fn themes(cx: &App) -> ThemeSelection {
    cx.try_global::<AppearanceState>()
        .map(|state| state.themes.clone())
        .unwrap_or_default()
}

pub fn accent(cx: &App) -> AccentSelection {
    cx.try_global::<AppearanceState>()
        .map(|state| state.accent)
        .unwrap_or_default()
}

pub fn surface(cx: &App) -> SurfacePreference {
    cx.try_global::<AppearanceState>()
        .map(|state| state.surface)
        .unwrap_or_default()
}

/// Change the user's preference, repaint if that changed the palette, and write
/// the choice to disk.
pub fn set_mode(mode: AppearanceMode, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let state = cx.global_mut::<AppearanceState>();
    if state.mode == mode {
        return;
    }
    state.mode = mode;
    apply(cx);
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.appearance = mode;
    });
}

/// Change the interactive accent without changing any semantic theme roles.
pub fn set_theme(appearance: Appearance, variant_id: impl Into<String>, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let variant_id = variant_id.into();
    let state = cx.global_mut::<AppearanceState>();
    if state.themes.variant_id(model_appearance(appearance)) == variant_id {
        return;
    }
    state
        .themes
        .set_variant(model_appearance(appearance), variant_id);
    let themes = state.themes.clone();
    apply(cx);
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.theme_selection = themes;
    });
}

pub fn set_accent(accent: AccentSelection, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let state = cx.global_mut::<AppearanceState>();
    if state.accent == accent {
        return;
    }
    state.accent = accent;
    apply(cx);
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.accent = accent;
    });
}

/// Change glass independently from appearance, theme, and accent selections.
pub fn set_surface(surface: SurfacePreference, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let state = cx.global_mut::<AppearanceState>();
    if state.surface == surface {
        return;
    }
    state.surface = surface;
    apply(cx);
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.surface = surface;
    });
}

/// Subscribe a window to OS appearance changes. The returned [`Subscription`]
/// must outlive the window — callers typically `.detach()` it.
///
/// The notification is *per window*, but the appearance it reports is a system
/// setting, so any one window is enough to learn about the change; re-applying
/// is idempotent when several fire.
pub fn observe_window(window: &mut Window, cx: &mut App) -> Subscription {
    // Reconcile against the *window's* appearance before subscribing.
    //
    // [`init`] runs before any window exists and can only ask the platform
    // (`App::window_appearance`), which on macOS reads `NSApp.effectiveAppearance`
    // — and that is not reliably populated that early in launch. When it guesses
    // wrong the app paints the wrong palette until some unrelated event happens to
    // fire the appearance notification, which reads as "it booted dark and fixed
    // itself when I clicked something". The window knows for certain, so ask it.
    sync(Appearance::from_window(window.appearance()), cx);
    window.observe_window_appearance(|window, cx| {
        sync(Appearance::from_window(window.appearance()), cx);
    })
}

/// Record the OS appearance and re-apply if it moved.
fn sync(system: Appearance, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let state = cx.global_mut::<AppearanceState>();
    if state.system == system {
        return;
    }
    tracing::debug!(?system, "appearance: system changed");
    state.system = system;
    apply(cx);
}

/// Re-resolve the palette and, if it moved, swap the theme and force a full
/// repaint. A no-op when the resolved appearance is unchanged — the OS fires the
/// notification for vibrancy and accent-color changes too, and repainting every
/// window for those would be a visible hitch for nothing.
pub fn apply(cx: &mut App) {
    let Some(state) = cx.try_global::<AppearanceState>() else {
        return;
    };
    sync_ns_appearance(state.mode);
    let wanted = resolve(state.mode, state.system);
    let accent = state.accent;
    let surface = state.surface;
    let variant_id = state.themes.variant_id(model_appearance(wanted)).to_owned();
    let changed = !cx.try_global::<Theme>().is_some_and(|theme| {
        theme.appearance == wanted
            && theme.variant_id.as_ref() == variant_id
            && theme.accent_selection == accent
            && theme.surface_preference == surface
    });
    if changed {
        tracing::debug!(?wanted, %variant_id, "appearance: installing palette");
        Theme::install_selection(wanted, &variant_id, accent, surface, cx);
        cx.refresh_windows();
    }
    // Unconditional, even when the palette did not move: this is the only thing
    // that keeps macOS vibrancy alive. gpui's macOS backend removes the
    // `NSVisualEffectView` from the window the moment the background appearance
    // is anything but `Blurred`, and nothing puts it back on its own — so a
    // single missed re-apply leaves the sidebar and tab strip permanently
    // opaque, which is exactly how the frost died. zed runs the same loop on
    // every settings change (`crates/zed/src/main.rs`).
    reapply_window_background(cx);
}

/// Rebuild the current palette after the installed custom registry changes.
/// Unlike [`apply`], this deliberately reinstalls even when the selected id is
/// unchanged because a linked theme may have recompiled under that same id.
pub fn apply_registry_change(cx: &mut App) {
    let Some(state) = cx.try_global::<AppearanceState>() else {
        return;
    };
    let wanted = resolve(state.mode, state.system);
    let accent = state.accent;
    let surface = state.surface;
    let variant_id = state.themes.variant_id(model_appearance(wanted)).to_owned();
    Theme::reinstall_selection(wanted, &variant_id, accent, surface, cx);
    cx.refresh_windows();
    reapply_window_background(cx);
}

fn model_appearance(appearance: Appearance) -> zeron_theme::Appearance {
    match appearance {
        Appearance::Dark => zeron_theme::Appearance::Dark,
        Appearance::Light => zeron_theme::Appearance::Light,
    }
}

/// Tell AppKit which appearance the app's windows use, so the chrome *it*
/// draws — the traffic lights above all — matches the palette *we* paint.
/// gpui never sets `NSAppearance`, so before this a pinned in-app theme left
/// the window chrome following the OS setting: a light window rendered
/// dark-appearance inactive traffic lights when the system was dark (user
/// report). Pinned modes name the appearance explicitly; `System` clears the
/// override (`setAppearance: nil`) so AppKit follows the OS again — resolving
/// to a name there too would freeze the chrome across OS sunset switches
/// until our own notification round-trip repainted it.
#[cfg(target_os = "macos")]
fn sync_ns_appearance(mode: AppearanceMode) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    // NSAppearanceName constants are NSStrings whose value equals the
    // constant's own name (AppKit documents them as stable identifiers), so
    // building them from literals avoids linking the extern statics.
    let name = match mode {
        AppearanceMode::System => None,
        AppearanceMode::Light => Some(c"NSAppearanceNameAqua"),
        AppearanceMode::Dark => Some(c"NSAppearanceNameDarkAqua"),
    };
    unsafe {
        let appearance: *mut Object = match name {
            None => std::ptr::null_mut(),
            Some(name) => {
                let name: *mut Object =
                    msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
                msg_send![class!(NSAppearance), appearanceNamed: name]
            }
        };
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setAppearance: appearance];
    }
}

#[cfg(not(target_os = "macos"))]
fn sync_ns_appearance(_mode: AppearanceMode) {}

/// Push the theme's window background appearance onto every open window.
pub fn reapply_window_background(cx: &mut App) {
    let Some(wanted) = cx
        .try_global::<Theme>()
        .map(|theme| theme.window_background_appearance())
    else {
        return;
    };
    for window in cx.windows() {
        window
            .update(cx, |_, window, _| {
                window.set_background_appearance(wanted);
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_mode_follows_the_os() {
        assert_eq!(
            resolve(AppearanceMode::System, Appearance::Light),
            Appearance::Light
        );
        assert_eq!(
            resolve(AppearanceMode::System, Appearance::Dark),
            Appearance::Dark
        );
    }

    #[test]
    fn pinned_modes_ignore_the_os() {
        for system in [Appearance::Light, Appearance::Dark] {
            assert_eq!(resolve(AppearanceMode::Light, system), Appearance::Light);
            assert_eq!(resolve(AppearanceMode::Dark, system), Appearance::Dark);
        }
    }

    #[test]
    fn default_mode_is_system() {
        assert_eq!(AppearanceMode::default(), AppearanceMode::System);
    }

    /// The setting round-trips through the settings file as a lowercase string.
    #[test]
    fn mode_serialises_stably() {
        for (mode, json) in [
            (AppearanceMode::System, "\"system\""),
            (AppearanceMode::Light, "\"light\""),
            (AppearanceMode::Dark, "\"dark\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<AppearanceMode>(json).unwrap(),
                mode,
                "{json} should parse back"
            );
        }
    }
}
