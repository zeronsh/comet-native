//! UI settings persisted to a small JSON file in the data dir — pane widths and
//! collapse flags (zeron persisted the same set in localStorage).
//!
//! Loaded once at boot and then owned by [`SettingsStore`], the only production
//! writer. Frequent geometry changes are debounced; durable choices flush
//! immediately through that same writer. Corrupt or missing files fall back to
//! defaults, and loaded values are clamped so a hand-edited file can't wedge the
//! layout.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, Global, Task};
use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod appearance;
pub mod archived;
pub mod composer;
pub mod devices;
pub mod harnesses;
pub mod notifications;
pub mod shortcuts;
pub mod widgets;

/// Sidebar drag-resize bounds (px).
pub const SIDEBAR_MIN: f32 = 208.0;
pub const SIDEBAR_MAX: f32 = 400.0;
pub const SIDEBAR_DEFAULT: f32 = 256.0;

/// Right ("Changes") pane drag-resize floor and default (px). Its runtime
/// maximum is the window space remaining after the left sidebar and the
/// conversation's [`CHAT_PANEL_MIN`] reservation.
pub const RIGHT_PANE_MIN: f32 = 360.0;
pub const RIGHT_PANE_DEFAULT: f32 = 520.0;
/// Minimum width retained for the conversation when the right pane is open.
pub const CHAT_PANEL_MIN: f32 = 300.0;

/// Terminal panel height bounds: 160px … 55% of the viewport (§1.10). The
/// viewport-relative cap applies at runtime; the absolute cap here only heals
/// hand-edited files.
pub const TERMINAL_MIN_HEIGHT: f32 = 160.0;
pub const TERMINAL_MAX_VH: f32 = 0.55;
pub const TERMINAL_ABS_MAX_HEIGHT: f32 = 2000.0;
pub const TERMINAL_DEFAULT_HEIGHT: f32 = 280.0;

/// Debounce for settings writes after a drag/toggle.
pub const SAVE_DEBOUNCE_MS: u64 = 400;

const FILE_NAME: &str = "ui-settings.json";

/// Whether a settings mutation should wait for the normal coalescing window or
/// reach disk before returning to the event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePolicy {
    Debounced,
    Immediate,
}

/// The sole in-process owner and writer of `ui-settings.json`.
///
/// Mutations land in `current` before any timer starts. Replacing a pending
/// task cancels its stale snapshot, and immediate mutations cancel the timer
/// before flushing synchronously.
pub struct SettingsStore {
    current: UiSettings,
    data_dir: PathBuf,
    revision: u64,
    saved_revision: u64,
    /// In-process invalidation token for transcript code-fence layout. Unlike
    /// the persisted revision, this advances only when the global Fit choice
    /// changes, including a change back to its previous value.
    code_fences_generation: u64,
    save_task: Option<Task<()>>,
}

impl Global for SettingsStore {}

impl SettingsStore {
    fn snapshot(&self) -> (UiSettings, u64) {
        (self.current.clone(), self.revision)
    }

    fn mark_saved(&mut self, revision: u64) -> bool {
        self.saved_revision = self.saved_revision.max(revision);
        self.saved_revision == self.revision
    }

    fn update_current(&mut self, mutate: impl FnOnce(&mut UiSettings)) -> bool {
        let before = self.current.clone();
        mutate(&mut self.current);
        self.current = self.current.clone().clamped();
        if self.current == before {
            return false;
        }
        if self.current.code_fences_fit_content != before.code_fences_fit_content {
            self.code_fences_generation = self.code_fences_generation.wrapping_add(1);
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }
}

pub fn init(settings: UiSettings, data_dir: impl Into<PathBuf>, cx: &mut App) {
    cx.set_global(SettingsStore {
        current: settings,
        data_dir: data_dir.into(),
        revision: 0,
        saved_revision: 0,
        code_fences_generation: 0,
        save_task: None,
    });
}

/// Latest settings, including mutations still inside the debounce window.
pub fn current(cx: &App) -> UiSettings {
    cx.try_global::<SettingsStore>()
        .map(|store| store.current.clone())
        .unwrap_or_default()
}

/// Monotonic id of the global code-fence layout choice. Every transcript
/// compares this during render so inactive subagent tabs can observe all mode
/// transitions when they next become visible.
pub fn code_fences_generation(cx: &App) -> u64 {
    cx.try_global::<SettingsStore>()
        .map(|store| store.code_fences_generation)
        .unwrap_or_default()
}

pub fn update(policy: SavePolicy, cx: &mut App, mutate: impl FnOnce(&mut UiSettings)) -> bool {
    if !cx.has_global::<SettingsStore>() {
        return false;
    }
    if !cx.global_mut::<SettingsStore>().update_current(mutate) {
        return false;
    }
    schedule(policy, cx);
    true
}

pub fn replace(settings: UiSettings, policy: SavePolicy, cx: &mut App) -> bool {
    update(policy, cx, |current| *current = settings)
}

fn schedule(policy: SavePolicy, cx: &mut App) {
    let old_task = cx.global_mut::<SettingsStore>().save_task.take();
    drop(old_task);

    match policy {
        SavePolicy::Immediate => flush(cx),
        SavePolicy::Debounced => {
            let task = cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                    .await;
                cx.update(flush_latest);
            });
            cx.global_mut::<SettingsStore>().save_task = Some(task);
        }
    }
}

/// Persist the latest revision. Safe to call at shutdown.
pub fn flush(cx: &mut App) {
    if !cx.has_global::<SettingsStore>() {
        return;
    }
    let pending = cx.global_mut::<SettingsStore>().save_task.take();
    drop(pending);
    flush_latest(cx);
}

fn flush_latest(cx: &mut App) {
    let Some(store) = cx.try_global::<SettingsStore>() else {
        return;
    };
    if store.saved_revision == store.revision {
        return;
    }
    let (settings, revision) = store.snapshot();
    let data_dir = store.data_dir.clone();
    match settings.save(&data_dir) {
        Ok(()) => {
            let current = cx.global_mut::<SettingsStore>().mark_saved(revision);
            debug_assert!(current, "foreground settings write cannot be overtaken");
        }
        Err(err) => tracing::warn!(error = %err, revision, "failed to persist ui settings"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SidebarOrganization {
    /// Legacy persisted value. Project scope now belongs exclusively to the
    /// project selector and is normalized to [`Self::InOneList`] on load.
    ByProject,
    ByDevice,
    #[default]
    InOneList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SidebarSort {
    #[default]
    LastUpdated,
    Created,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// Legacy: the grouped-by-project toggle predates spaces (which group by
    /// folder inherently). Kept for file compatibility; no longer read.
    pub sidebar_grouped: bool,
    /// How active sessions are partitioned in the sidebar.
    pub sidebar_organization: SidebarOrganization,
    /// Timestamp used to order active sessions (newest first).
    pub sidebar_sort: SidebarSort,
    /// Optional harness branding and repository metadata shown below each
    /// session title.
    pub sidebar_show_harness: bool,
    pub sidebar_show_branch: bool,
    pub sidebar_show_pull_request: bool,
    /// The last selected space — restored on boot when the row still exists;
    /// also the new-tab default when the sidebar filter is "All".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_space_id: Option<String>,
    /// Open session tabs in visual order (drag-reorder edits in place).
    /// Device-local: a tab is a local viewport onto the synced session list —
    /// closing one never archives the session. Ids of archived/deleted chats
    /// are pruned against the doc ([`Shell::sync_open_tabs`]). `None` = file
    /// written by a pre-tabs build; seeded once from the last space's sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_tabs: Option<Vec<String>>,
    /// Sidebar session filter: a space id, or `None` for "All spaces".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_filter: Option<String>,
    /// Legacy: per-space tab order, from when tabs were the selected space's
    /// non-archived sessions. Kept for file compatibility; no longer read.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tab_order: std::collections::HashMap<String, Vec<String>>,
    /// Legacy: manual sidebar space order, from when spaces were a sidebar
    /// list. Kept for file compatibility; no longer read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub space_order: Vec<String>,
    /// Session notification chimes (done / awaiting-input). `ZERON_DISABLE_SOUND`
    /// overrides.
    pub sound_enabled: bool,
    /// Desktop banner notifications on the same transitions.
    /// `ZERON_DISABLE_NOTIFICATIONS` overrides.
    pub notifications_enabled: bool,
    /// Suppress the banner while a Zeron window is focused (the chime covers
    /// the foreground case).
    pub notifications_background_only: bool,
    pub right_pane_width: f32,
    /// Legacy: panel *open* flags are session-scoped in-memory state now
    /// (`shell::SessionPanels`, zeron `sessionPanels` parity). Kept for file
    /// compatibility; no longer read or written by the shell.
    pub right_pane_open: bool,
    pub terminal_height: f32,
    /// Legacy — see [`Self::right_pane_open`].
    pub terminal_open: bool,
    /// Customizable shortcut combos (feature-inventory §1.4).
    pub keymap: KeymapConfig,
    /// Light/dark preference. Defaults to following the OS.
    pub appearance: crate::appearance::AppearanceMode,
    /// Interface and conversational-prose family. Device-local by design.
    pub ui_font_family: crate::typography::UiFontFamily,
    /// Base size for interface and conversational prose. Code-related surfaces
    /// retain their fixed metrics.
    pub ui_font_size: crate::typography::UiFontSize,
    /// Independently selected light and dark theme variants.
    pub theme_selection: zeron_theme::ThemeSelection,
    /// Changes pane: side-by-side diffs instead of the unified stack.
    pub diff_split: bool,
    /// Changes pane: wrap long source lines instead of scrolling horizontally.
    pub diff_wrap: bool,
    /// Agent-sent Markdown fences: wrap long lines to the chat width instead
    /// of exposing their horizontal scroll plane.
    pub code_fences_fit_content: bool,
    /// Interactive identity overlay; imported themes default to their own accent.
    pub accent: zeron_theme::AccentSelection,
    /// Glass policy, independent from the selected appearance, theme, and accent.
    pub surface: zeron_theme::SurfacePreference,
    /// Pre-theme settings used `accentColor`. Read it once, migrate to
    /// [`Self::accent`], and never write it again.
    #[serde(default, rename = "accentColor", skip_serializing)]
    legacy_accent_color: Option<crate::theme::AccentColor>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            sidebar_grouped: false,
            sidebar_organization: SidebarOrganization::InOneList,
            sidebar_sort: SidebarSort::LastUpdated,
            sidebar_show_harness: true,
            sidebar_show_branch: true,
            sidebar_show_pull_request: true,
            last_space_id: None,
            open_tabs: None,
            space_filter: None,
            tab_order: std::collections::HashMap::new(),
            space_order: Vec::new(),
            sound_enabled: true,
            notifications_enabled: true,
            notifications_background_only: true,
            right_pane_width: RIGHT_PANE_DEFAULT,
            right_pane_open: false,
            terminal_height: TERMINAL_DEFAULT_HEIGHT,
            terminal_open: false,
            keymap: KeymapConfig::default(),
            appearance: crate::appearance::AppearanceMode::default(),
            ui_font_family: crate::typography::UiFontFamily::default(),
            ui_font_size: crate::typography::UiFontSize::default(),
            theme_selection: zeron_theme::ThemeSelection::default(),
            diff_split: false,
            diff_wrap: false,
            code_fences_fit_content: false,
            accent: zeron_theme::AccentSelection::default(),
            surface: zeron_theme::SurfacePreference::default(),
            legacy_accent_color: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Keymap (customizable shortcuts, §1.4)
// ---------------------------------------------------------------------------

/// How many sidebar rows the jump shortcuts reach (t3code's
/// `THREAD_JUMP_KEYBINDING_COMMANDS`, nine slots).
pub const JUMP_SLOTS: usize = 9;

/// Default combo per jump slot, and the label the shortcuts table shows.
const JUMP_DEFAULTS: [&str; JUMP_SLOTS] = [
    "mod-1", "mod-2", "mod-3", "mod-4", "mod-5", "mod-6", "mod-7", "mod-8", "mod-9",
];
const JUMP_LABELS: [&str; JUMP_SLOTS] = [
    "Jump to session 1",
    "Jump to session 2",
    "Jump to session 3",
    "Jump to session 4",
    "Jump to session 5",
    "Jump to session 6",
    "Jump to session 7",
    "Jump to session 8",
    "Jump to session 9",
];

/// The rebindable app shortcuts. `JumpSession(slot)` is zero-based; a slot at
/// or past [`JUMP_SLOTS`] has no combo and no label, so it reads as unbound
/// rather than panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutId {
    ToggleSidebar,
    ToggleChanges,
    ToggleTerminal,
    NewSession,
    NextSession,
    PrevSession,
    ArchiveSession,
    JumpSession(usize),
}

impl ShortcutId {
    pub const ALL: [ShortcutId; 7 + JUMP_SLOTS] = [
        ShortcutId::ToggleSidebar,
        ShortcutId::ToggleChanges,
        ShortcutId::ToggleTerminal,
        ShortcutId::NewSession,
        ShortcutId::NextSession,
        ShortcutId::PrevSession,
        ShortcutId::ArchiveSession,
        ShortcutId::JumpSession(0),
        ShortcutId::JumpSession(1),
        ShortcutId::JumpSession(2),
        ShortcutId::JumpSession(3),
        ShortcutId::JumpSession(4),
        ShortcutId::JumpSession(5),
        ShortcutId::JumpSession(6),
        ShortcutId::JumpSession(7),
        ShortcutId::JumpSession(8),
    ];

    /// Row label (zeron lib/shortcuts.ts `SHORTCUT_DEFINITIONS`, verbatim).
    pub fn label(self) -> &'static str {
        match self {
            ShortcutId::ToggleSidebar => "Toggle left sidebar",
            ShortcutId::ToggleChanges => "Toggle right sidebar",
            ShortcutId::ToggleTerminal => "Toggle terminal",
            ShortcutId::NewSession => "New session",
            ShortcutId::NextSession => "Next session",
            ShortcutId::PrevSession => "Previous session",
            ShortcutId::ArchiveSession => "Archive session",
            ShortcutId::JumpSession(slot) => JUMP_LABELS.get(slot).copied().unwrap_or(""),
        }
    }

    pub fn default_combo(self) -> &'static str {
        self.default_combo_on(cfg!(target_os = "macos"))
    }

    /// `default_combo` for an explicit platform, so the spelling invariant is
    /// testable for both from any machine (see the tests below — the mismatch
    /// this guards against only exists off macOS).
    pub fn default_combo_on(self, mac: bool) -> &'static str {
        match self {
            ShortcutId::ToggleSidebar => "mod-s",
            ShortcutId::ToggleChanges => "mod-b",
            ShortcutId::ToggleTerminal => "mod-j",
            ShortcutId::NewSession => "mod-n",
            // Ctrl+Tab on every platform — but spelled the way THAT platform's
            // recorder spells ctrl (see `combo_from_keystroke`). Off macOS
            // ctrl IS the primary and stores as "mod"; on macOS it is its own
            // modifier, and "mod" would mean Cmd+Tab, which the OS app
            // switcher eats.
            //
            // Off macOS "ctrl-tab" and "mod-tab" resolve to the same keystroke
            // through `platform_combo`, but conflict detection compares the
            // STORED spelling — so a default the recorder cannot reproduce
            // would let a rebind onto that same physical key pass as
            // conflict-free, bind twice, and silently kill one shortcut.
            ShortcutId::NextSession if mac => "ctrl-tab",
            ShortcutId::NextSession => "mod-tab",
            ShortcutId::PrevSession if mac => "ctrl-shift-tab",
            ShortcutId::PrevSession => "mod-shift-tab",
            // Mod+A is the composer's Select all, so archiving takes the
            // shifted combo.
            ShortcutId::ArchiveSession => "mod-shift-a",
            ShortcutId::JumpSession(slot) => JUMP_DEFAULTS.get(slot).copied().unwrap_or(""),
        }
    }

    /// The sidebar row this id jumps to, if it is a jump shortcut.
    pub fn jump_slot(self) -> Option<usize> {
        match self {
            ShortcutId::JumpSession(slot) if slot < JUMP_SLOTS => Some(slot),
            _ => None,
        }
    }
}

/// Persisted shortcut combos. Stored platform-neutral ("mod-s"); translated to
/// "cmd-s"/"ctrl-s" at bind time by [`platform_combo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeymapConfig {
    pub toggle_sidebar: String,
    pub toggle_changes: String,
    pub toggle_terminal: String,
    pub new_session: String,
    pub next_session: String,
    pub prev_session: String,
    pub archive_session: String,
    /// One combo per jump slot, in slot order. A list rather than nine fields:
    /// [`UiSettings::load`] discards the WHOLE file on a parse error, so a
    /// fixed-length array would let one malformed entry reset every unrelated
    /// setting. [`Self::healed`] restores the length instead.
    pub jump_session: Vec<String>,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            toggle_sidebar: ShortcutId::ToggleSidebar.default_combo().into(),
            toggle_changes: ShortcutId::ToggleChanges.default_combo().into(),
            toggle_terminal: ShortcutId::ToggleTerminal.default_combo().into(),
            new_session: ShortcutId::NewSession.default_combo().into(),
            next_session: ShortcutId::NextSession.default_combo().into(),
            prev_session: ShortcutId::PrevSession.default_combo().into(),
            archive_session: ShortcutId::ArchiveSession.default_combo().into(),
            jump_session: JUMP_DEFAULTS.iter().map(|c| (*c).to_string()).collect(),
        }
    }
}

impl KeymapConfig {
    pub fn get(&self, id: ShortcutId) -> &str {
        match id {
            ShortcutId::ToggleSidebar => &self.toggle_sidebar,
            ShortcutId::ToggleChanges => &self.toggle_changes,
            ShortcutId::ToggleTerminal => &self.toggle_terminal,
            ShortcutId::NewSession => &self.new_session,
            ShortcutId::NextSession => &self.next_session,
            ShortcutId::PrevSession => &self.prev_session,
            ShortcutId::ArchiveSession => &self.archive_session,
            ShortcutId::JumpSession(slot) => self
                .jump_session
                .get(slot)
                .map(String::as_str)
                .unwrap_or(""),
        }
    }

    pub fn set(&mut self, id: ShortcutId, combo: String) {
        match id {
            ShortcutId::ToggleSidebar => self.toggle_sidebar = combo,
            ShortcutId::ToggleChanges => self.toggle_changes = combo,
            ShortcutId::ToggleTerminal => self.toggle_terminal = combo,
            ShortcutId::NewSession => self.new_session = combo,
            ShortcutId::NextSession => self.next_session = combo,
            ShortcutId::PrevSession => self.prev_session = combo,
            ShortcutId::ArchiveSession => self.archive_session = combo,
            ShortcutId::JumpSession(slot) => {
                if slot < JUMP_SLOTS {
                    if self.jump_session.len() < JUMP_SLOTS {
                        self.heal_jump_slots();
                    }
                    self.jump_session[slot] = combo;
                }
            }
        }
    }

    pub fn reset(&mut self, id: ShortcutId) {
        self.set(id, id.default_combo().to_string());
    }

    /// Restore the jump list to exactly [`JUMP_SLOTS`] entries: a hand-edited
    /// or older file may carry a short, long or absent list. Surviving entries
    /// keep their slot; missing ones take the default.
    pub fn heal_jump_slots(&mut self) {
        self.jump_session.truncate(JUMP_SLOTS);
        while self.jump_session.len() < JUMP_SLOTS {
            self.jump_session
                .push(JUMP_DEFAULTS[self.jump_session.len()].to_string());
        }
    }
}

/// Build a combo string from a recorded keystroke. The primary modifier
/// (cmd on macOS, ctrl elsewhere) becomes "mod"; bare modifier presses record
/// nothing.
pub fn combo_from_keystroke(
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    key: &str,
) -> Option<String> {
    combo_from_keystroke_on(cfg!(target_os = "macos"), ctrl, alt, shift, cmd, key)
}

/// [`combo_from_keystroke`] for an explicit platform — the ctrl spelling is
/// platform-dependent, so both paths need to be exercisable from one machine.
pub fn combo_from_keystroke_on(
    mac: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    key: &str,
) -> Option<String> {
    let key = key.trim().to_lowercase();
    if key.is_empty()
        || matches!(
            key.as_str(),
            "ctrl" | "control" | "alt" | "shift" | "cmd" | "platform" | "fn"
        )
    {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    // On macOS ctrl stays its own modifier rather than folding into "mod":
    // re-recording Ctrl+Tab as Cmd+Tab would hand the combo to the OS app
    // switcher, which never delivers it to the window.
    let ctrl_is_primary = ctrl && !mac;
    if cmd || ctrl_is_primary {
        parts.push("mod");
    }
    if ctrl && !ctrl_is_primary {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    parts.push(&key);
    Some(parts.join("-"))
}

/// Shortcut ids whose combos collide with another shortcut (conflict detection).
pub fn conflicted_shortcuts(keymap: &KeymapConfig) -> Vec<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .filter(|&id| {
            let combo = keymap.get(id);
            !combo.is_empty()
                && ShortcutId::ALL
                    .into_iter()
                    .any(|other| other != id && keymap.get(other) == combo)
        })
        .collect()
}

/// The modifiers a stored combo carries, as `(mod, alt, shift)`. Everything
/// before the final segment is a modifier; the final segment is the key.
pub fn combo_modifiers(combo: &str) -> (bool, bool, bool) {
    let mut parts: Vec<&str> = combo.split('-').collect();
    parts.pop();
    (
        parts.contains(&"mod"),
        parts.contains(&"alt"),
        parts.contains(&"shift"),
    )
}

/// Whether the sidebar should show its jump hints for the currently held
/// modifiers (t3code `shouldShowThreadJumpHintsForModifiers`). The held set
/// must match a jump combo EXACTLY, so adding Shift or Alt hides the hints and
/// a chord like Cmd+Shift+4 never flashes the overlay. `primary` is the held
/// "mod" key — cmd on macOS, ctrl elsewhere.
///
/// A jump combo with no modifiers at all never shows hints: it would otherwise
/// match the resting state and pin the overlay open. Pure.
pub fn jump_hints_visible(keymap: &KeymapConfig, primary: bool, alt: bool, shift: bool) -> bool {
    if !(primary || alt || shift) {
        return false;
    }
    ShortcutId::ALL
        .into_iter()
        .filter(|id| id.jump_slot().is_some())
        .any(|id| combo_modifiers(keymap.get(id)) == (primary, alt, shift))
}

/// Translate a stored combo into a bindable keystroke for this platform.
pub fn platform_combo(combo: &str) -> String {
    platform_combo_on(cfg!(target_os = "macos"), combo)
}

/// [`platform_combo`] for an explicit platform (see [`combo_from_keystroke_on`]).
pub fn platform_combo_on(mac: bool, combo: &str) -> String {
    let primary = if mac { "cmd" } else { "ctrl" };
    combo
        .split('-')
        .map(|part| if part == "mod" { primary } else { part })
        .collect::<Vec<_>>()
        .join("-")
}

/// Human-readable combo for the shortcuts table ("mod-s" → "Cmd+S"/"Ctrl+S").
pub fn display_combo(combo: &str) -> String {
    display_combo_on(cfg!(target_os = "macos"), combo)
}

/// [`display_combo`] for an explicit platform (see [`combo_from_keystroke_on`]).
pub fn display_combo_on(mac: bool, combo: &str) -> String {
    combo
        .split('-')
        .map(|part| match part {
            "mod" => if mac { "Cmd" } else { "Ctrl" }.to_string(),
            "alt" => if mac { "Opt" } else { "Alt" }.to_string(),
            "shift" => "Shift".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Compact combo for badge surfaces (the sidebar jump hints): macOS spells
/// the modifiers as their key glyphs in canonical ⌃⌥⇧⌘ order and drops the
/// separators ("⌘1", "⇧⌘A") — the form the model picker's ⌘N chips already
/// use — while other platforms keep the textual [`display_combo`] ("Ctrl+1").
pub fn badge_combo(combo: &str) -> String {
    badge_combo_on(cfg!(target_os = "macos"), combo)
}

/// [`badge_combo`] for an explicit platform (see [`combo_from_keystroke_on`]).
pub fn badge_combo_on(mac: bool, combo: &str) -> String {
    if !mac {
        return display_combo_on(false, combo);
    }
    let mut parts: Vec<&str> = combo.split('-').collect();
    let key = parts.pop().unwrap_or("");
    let mut out = String::new();
    for glyph in ["ctrl", "alt", "shift", "mod"]
        .iter()
        .zip(['⌃', '⌥', '⇧', '⌘'])
        .filter_map(|(name, glyph)| parts.contains(name).then_some(glyph))
    {
        out.push(glyph);
    }
    let mut chars = key.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}

impl UiSettings {
    /// Clamp widths into their legal ranges (also heals NaN to defaults).
    pub fn clamped(mut self) -> Self {
        if self.sidebar_organization == SidebarOrganization::ByProject {
            self.sidebar_organization = SidebarOrganization::InOneList;
        }
        self.sidebar_width = clamp_or(
            self.sidebar_width,
            SIDEBAR_MIN,
            SIDEBAR_MAX,
            SIDEBAR_DEFAULT,
        );
        // The right pane has no persisted upper bound: its live drag clamps
        // against the current window, which is unavailable while loading.
        self.right_pane_width = min_or(self.right_pane_width, RIGHT_PANE_MIN, RIGHT_PANE_DEFAULT);
        self.terminal_height = clamp_or(
            self.terminal_height,
            TERMINAL_MIN_HEIGHT,
            TERMINAL_ABS_MAX_HEIGHT,
            TERMINAL_DEFAULT_HEIGHT,
        );
        self.ui_font_size = self.ui_font_size.normalized();
        self.keymap.heal_jump_slots();
        self
    }

    /// Load from `{data_dir}/ui-settings.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<UiSettings>(&text) {
                Ok(settings) => settings.migrated().clamped(),
                Err(err) => {
                    tracing::warn!(error = %err, "ui-settings corrupt; using defaults");
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

    fn migrated(mut self) -> Self {
        if self.accent == zeron_theme::AccentSelection::ThemeDefault
            && let Some(accent) = self.legacy_accent_color.take()
        {
            self.accent = zeron_theme::AccentSelection::Preset(accent.into());
        }
        self.legacy_accent_color = None;
        self
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

fn clamp_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

fn min_or(value: f32, min: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.max(min)
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = UiSettings {
            sidebar_width: 300.0,
            sidebar_collapsed: true,
            sidebar_grouped: true,
            sidebar_organization: SidebarOrganization::ByDevice,
            sidebar_sort: SidebarSort::Created,
            sidebar_show_harness: false,
            sidebar_show_branch: false,
            sidebar_show_pull_request: false,
            last_space_id: Some("space-1".into()),
            open_tabs: Some(vec!["b".to_string(), "a".to_string()]),
            space_filter: Some("space-1".into()),
            tab_order: std::collections::HashMap::from([(
                "space-1".to_string(),
                vec!["b".to_string(), "a".to_string()],
            )]),
            space_order: vec!["space-2".to_string(), "space-1".to_string()],
            sound_enabled: false,
            notifications_enabled: false,
            notifications_background_only: false,
            right_pane_width: 700.0,
            right_pane_open: true,
            terminal_height: 320.0,
            terminal_open: true,
            keymap: KeymapConfig {
                toggle_sidebar: "mod-shift-s".into(),
                ..KeymapConfig::default()
            },
            appearance: crate::appearance::AppearanceMode::Light,
            ui_font_family: crate::typography::UiFontFamily::Installed("Arial".into()),
            ui_font_size: crate::typography::UiFontSize::ALL[5],
            theme_selection: zeron_theme::ThemeSelection {
                light: "catppuccin-latte".into(),
                dark: "catppuccin-mocha".into(),
            },
            diff_split: true,
            diff_wrap: true,
            code_fences_fit_content: true,
            accent: zeron_theme::AccentSelection::Preset(zeron_theme::AccentPreset::Cyan),
            surface: zeron_theme::SurfacePreference::Frosted,
            legacy_accent_color: None,
        };
        settings.save(dir.path()).unwrap();
        let json = std::fs::read_to_string(UiSettings::path(dir.path())).unwrap();
        assert!(json.contains(r#""diffWrap": true"#));
        assert_eq!(UiSettings::load(dir.path()), settings);
        assert!(json.contains(r#""codeFencesFitContent": true"#));
    }

    #[test]
    fn stale_revision_cannot_be_considered_the_latest_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore {
            current: UiSettings::default(),
            data_dir: dir.path().to_path_buf(),
            revision: 0,
            saved_revision: 0,
            code_fences_generation: 0,
            save_task: None,
        };

        store.current.sidebar_width = 300.0;
        store.revision += 1;
        let (stale, stale_revision) = store.snapshot();

        store.current.ui_font_family = crate::typography::UiFontFamily::Installed("Arial".into());
        store.revision += 1;
        stale.save(dir.path()).unwrap();
        assert!(!store.mark_saved(stale_revision));

        let (latest, latest_revision) = store.snapshot();
        latest.save(dir.path()).unwrap();
        assert!(store.mark_saved(latest_revision));
        let reloaded = UiSettings::load(dir.path());
        assert_eq!(reloaded.sidebar_width, 300.0);
        assert_eq!(
            reloaded.ui_font_family,
            crate::typography::UiFontFamily::Installed("Arial".into())
        );
    }

    #[test]
    fn code_fence_generation_tracks_every_mode_transition_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore {
            current: UiSettings::default(),
            data_dir: dir.path().to_path_buf(),
            revision: 0,
            saved_revision: 0,
            code_fences_generation: 0,
            save_task: None,
        };

        assert!(store.update_current(|settings| settings.sidebar_width = 300.0));
        assert_eq!(store.code_fences_generation, 0);

        assert!(store.update_current(|settings| settings.code_fences_fit_content = true));
        assert_eq!(store.code_fences_generation, 1);
        assert!(store.update_current(|settings| settings.code_fences_fit_content = false));
        assert_eq!(store.code_fences_generation, 2);

        assert!(!store.update_current(|settings| settings.code_fences_fit_content = false));
        assert_eq!(store.code_fences_generation, 2);
    }

    #[test]
    fn legacy_project_organization_normalizes_to_one_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarOrganization":"byProject"}"#,
        )
        .unwrap();

        assert_eq!(
            UiSettings::load(dir.path()).sidebar_organization,
            SidebarOrganization::InOneList
        );
    }

    /// A settings file written before light mode existed has no `appearance`
    /// key; it must load as "follow the OS" rather than failing the whole parse
    /// and resetting every other preference to defaults.
    #[test]
    fn settings_without_appearance_default_to_system() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.appearance, crate::appearance::AppearanceMode::System);
        assert_eq!(loaded.accent, zeron_theme::AccentSelection::ThemeDefault);
        assert_eq!(loaded.surface, zeron_theme::SurfacePreference::ThemeDefault);
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled, "other keys still parse");
        assert!(
            loaded.notifications_enabled,
            "pre-banner files default banners on"
        );
        assert!(
            loaded.notifications_background_only,
            "pre-banner files default background-only on"
        );
    }

    #[test]
    fn legacy_accent_color_migrates_to_an_explicit_preset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"accentColor":"cyan"}"#).unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(
            loaded.accent,
            zeron_theme::AccentSelection::Preset(zeron_theme::AccentPreset::Cyan)
        );
        loaded.save(dir.path()).unwrap();
        let saved = std::fs::read_to_string(UiSettings::path(dir.path())).unwrap();
        assert!(!saved.contains("accentColor"));
    }

    #[test]
    fn settings_without_ui_font_default_to_geist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(
            loaded.ui_font_family,
            crate::typography::UiFontFamily::Geist
        );
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled);
        assert!(!loaded.diff_wrap);
        assert_eq!(
            loaded.ui_font_size,
            crate::typography::UiFontSize::default()
        );
    }

    #[test]
    fn unsupported_ui_font_size_snaps_to_the_nearest_choice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"uiFontSize": 19, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.ui_font_size.pixels(), 18.0);
        assert!(!loaded.sound_enabled);
    }

    #[test]
    fn unknown_ui_font_falls_back_without_resetting_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "uiFontFamily": "futureSans"}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(
            loaded.ui_font_family,
            crate::typography::UiFontFamily::Geist
        );
        assert_eq!(loaded.sidebar_width, 300.0);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
        std::fs::write(UiSettings::path(dir.path()), "{not json").unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
    }

    #[test]
    fn loaded_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 10000, "rightPaneWidth": 1}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, SIDEBAR_MAX);
        assert_eq!(loaded.right_pane_width, RIGHT_PANE_MIN);
        assert!(!loaded.code_fences_fit_content);
    }

    #[test]
    fn large_right_pane_width_is_preserved() {
        let loaded = UiSettings {
            right_pane_width: 2400.0,
            ..Default::default()
        }
        .clamped();
        assert_eq!(loaded.right_pane_width, 2400.0);
    }

    #[test]
    fn nan_heals_to_default() {
        let healed = UiSettings {
            sidebar_width: f32::NAN,
            ..Default::default()
        }
        .clamped();
        assert_eq!(healed.sidebar_width, SIDEBAR_DEFAULT);
    }

    #[test]
    fn defaults_match_zeron() {
        let d = UiSettings::default();
        assert_eq!(d.sidebar_width, 256.0);
        assert_eq!(d.right_pane_width, 520.0);
        assert_eq!(d.terminal_height, 280.0);
        assert!(!d.sidebar_collapsed && !d.right_pane_open && !d.terminal_open);
    }

    #[test]
    fn keymap_defaults_and_reset() {
        let mut keymap = KeymapConfig::default();
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-s");
        assert_eq!(keymap.get(ShortcutId::ToggleChanges), "mod-b");
        assert_eq!(keymap.get(ShortcutId::ToggleTerminal), "mod-j");
        let ctrl = if cfg!(target_os = "macos") {
            "ctrl"
        } else {
            "mod"
        };
        assert_eq!(keymap.get(ShortcutId::NextSession), format!("{ctrl}-tab"));
        assert_eq!(
            keymap.get(ShortcutId::PrevSession),
            format!("{ctrl}-shift-tab")
        );
        assert_eq!(keymap.get(ShortcutId::NewSession), "mod-n");
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-a");
        keymap.set(ShortcutId::ToggleSidebar, "mod-shift-x".into());
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        keymap.reset(ShortcutId::ToggleSidebar);
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-s");
        keymap.set(ShortcutId::ArchiveSession, "mod-shift-y".into());
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-y");
        keymap.reset(ShortcutId::ArchiveSession);
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-a");
    }

    #[test]
    fn every_shortcut_default_is_unique_and_bindable() {
        // A new shortcut must not ship in conflict with an existing one, and
        // its default must parse on this platform.
        assert!(conflicted_shortcuts(&KeymapConfig::default()).is_empty());
        for id in ShortcutId::ALL {
            assert!(
                gpui::Keystroke::parse(&platform_combo(id.default_combo())).is_ok(),
                "{:?} default combo does not parse",
                id
            );
        }
    }

    #[test]
    fn combo_recording() {
        // How this platform spells a recorded ctrl (see `combo_from_keystroke_on`).
        let ctrl_combo = |suffix: &str| {
            if cfg!(target_os = "macos") {
                format!("ctrl-{suffix}")
            } else {
                format!("mod-{suffix}")
            }
        };
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "s"),
            Some(ctrl_combo("s"))
        );
        assert_eq!(
            combo_from_keystroke(false, false, false, true, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(true, false, true, false, "tab"),
            Some(ctrl_combo("shift-tab"))
        );
        assert_eq!(
            combo_from_keystroke(true, true, true, false, "K"),
            Some(ctrl_combo("alt-shift-k"))
        );
        // Plain keys record without modifiers (Esc is filtered by the caller).
        assert_eq!(
            combo_from_keystroke(false, false, false, false, "f5"),
            Some("f5".into())
        );
        // Bare modifier presses record nothing.
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "ctrl"),
            None
        );
        assert_eq!(
            combo_from_keystroke(false, false, true, false, "shift"),
            None
        );
        assert_eq!(combo_from_keystroke(false, false, false, false, ""), None);
    }

    #[test]
    fn every_default_is_spelled_the_way_the_recorder_spells_it() {
        // The invariant `default_combo_on` documents. Checked for BOTH
        // platforms because the hazard only exists off macOS, so a single-OS
        // CI run would never see it.
        for mac in [true, false] {
            for id in ShortcutId::ALL {
                let combo = id.default_combo_on(mac);
                // Via the platform spelling, where modifier names are
                // unambiguous, so the decode can't inherit the bug it checks.
                let bound = platform_combo_on(mac, combo);
                let mut parts: Vec<&str> = bound.split('-').collect();
                let key = parts.pop().expect("a combo always ends in a key");
                let recorded = combo_from_keystroke_on(
                    mac,
                    parts.contains(&"ctrl"),
                    parts.contains(&"alt"),
                    parts.contains(&"shift"),
                    parts.contains(&"cmd"),
                    key,
                );
                assert_eq!(
                    recorded.as_deref(),
                    Some(combo),
                    "{} default {combo:?} is unreachable from the recorder (mac={mac})",
                    id.label()
                );
            }
        }
    }

    #[test]
    fn defaults_are_distinct_physical_keys() {
        // Distinct STRINGS is not enough — two defaults could still resolve to
        // the same keystroke through `platform_combo`.
        let mut seen = std::collections::HashSet::new();
        for id in ShortcutId::ALL {
            let bound = platform_combo(id.default_combo());
            assert!(seen.insert(bound.clone()), "{bound:?} bound twice");
        }
    }

    #[test]
    fn conflict_detection() {
        let mut keymap = KeymapConfig::default();
        assert!(conflicted_shortcuts(&keymap).is_empty());
        keymap.set(ShortcutId::ToggleChanges, "mod-s".into());
        let conflicts = conflicted_shortcuts(&keymap);
        assert!(conflicts.contains(&ShortcutId::ToggleSidebar));
        assert!(conflicts.contains(&ShortcutId::ToggleChanges));
        assert!(!conflicts.contains(&ShortcutId::ToggleTerminal));
        keymap.reset(ShortcutId::ToggleChanges);
        assert!(conflicted_shortcuts(&keymap).is_empty());
    }

    #[test]
    fn combo_translation() {
        let primary = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        assert_eq!(platform_combo("mod-s"), format!("{primary}-s"));
        assert_eq!(platform_combo("alt-f4"), "alt-f4");
        let display_primary = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
        assert_eq!(
            display_combo("mod-shift-s"),
            format!("{display_primary}+Shift+S")
        );
        assert_eq!(display_combo("f5"), "F5");
        assert_eq!(display_combo_on(true, "mod-alt-up"), "Cmd+Opt+Up");
        assert_eq!(display_combo_on(false, "mod-alt-up"), "Ctrl+Alt+Up");
        // Literal ctrl passes through untouched — the macOS spelling of
        // session cycling.
        assert_eq!(platform_combo("ctrl-shift-tab"), "ctrl-shift-tab");
        assert_eq!(display_combo("ctrl-shift-tab"), "Ctrl+Shift+Tab");
    }

    #[test]
    fn keymap_survives_old_settings_files() {
        // Files written before the keymap existed load with defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"sidebarWidth": 300}"#).unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.keymap, KeymapConfig::default());
        assert!(!loaded.sidebar_grouped);
    }

    #[test]
    fn jump_slots_get_set_and_reset() {
        let mut keymap = KeymapConfig::default();
        assert_eq!(keymap.get(ShortcutId::JumpSession(0)), "mod-1");
        assert_eq!(keymap.get(ShortcutId::JumpSession(8)), "mod-9");
        // Past the last slot there is no shortcut, not a panic.
        assert_eq!(keymap.get(ShortcutId::JumpSession(9)), "");
        assert_eq!(ShortcutId::JumpSession(9).jump_slot(), None);
        assert_eq!(ShortcutId::JumpSession(0).jump_slot(), Some(0));
        assert_eq!(ShortcutId::ArchiveSession.jump_slot(), None);

        keymap.set(ShortcutId::JumpSession(2), "mod-alt-3".into());
        assert_eq!(keymap.get(ShortcutId::JumpSession(2)), "mod-alt-3");
        keymap.reset(ShortcutId::JumpSession(2));
        assert_eq!(keymap.get(ShortcutId::JumpSession(2)), "mod-3");
        // A write past the last slot is dropped, and grows nothing.
        keymap.set(ShortcutId::JumpSession(9), "mod-0".into());
        assert_eq!(keymap.jump_session.len(), JUMP_SLOTS);
    }

    #[test]
    fn short_or_long_jump_lists_heal_to_the_slot_count() {
        // Short: surviving entries keep their slot, the rest take defaults.
        let mut keymap = KeymapConfig {
            jump_session: vec!["mod-alt-1".into()],
            ..KeymapConfig::default()
        };
        keymap.heal_jump_slots();
        assert_eq!(keymap.jump_session.len(), JUMP_SLOTS);
        assert_eq!(keymap.get(ShortcutId::JumpSession(0)), "mod-alt-1");
        assert_eq!(keymap.get(ShortcutId::JumpSession(1)), "mod-2");

        // Long: the tail is dropped.
        let mut keymap = KeymapConfig {
            jump_session: (0..20).map(|i| format!("mod-{i}")).collect(),
            ..KeymapConfig::default()
        };
        keymap.heal_jump_slots();
        assert_eq!(keymap.jump_session.len(), JUMP_SLOTS);

        // Absent: the whole list comes back.
        let mut keymap = KeymapConfig {
            jump_session: Vec::new(),
            ..KeymapConfig::default()
        };
        keymap.heal_jump_slots();
        assert_eq!(keymap.jump_session, KeymapConfig::default().jump_session);
    }

    #[test]
    fn a_malformed_jump_list_heals_without_losing_other_settings() {
        // Healing happens on load, so an odd jumpSession must not cost the
        // user their sidebar width or their other combos.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "keymap": {"toggleSidebar": "mod-shift-x", "jumpSession": ["mod-alt-1", "mod-alt-2"]}}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, 300.0);
        assert_eq!(loaded.keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        assert_eq!(loaded.keymap.get(ShortcutId::JumpSession(0)), "mod-alt-1");
        assert_eq!(loaded.keymap.get(ShortcutId::JumpSession(8)), "mod-9");
        assert_eq!(loaded.keymap.jump_session.len(), JUMP_SLOTS);
    }

    #[test]
    fn jump_hints_need_an_exact_modifier_match() {
        let keymap = KeymapConfig::default();
        // Mod alone matches mod-1..9.
        assert!(jump_hints_visible(&keymap, true, false, false));
        // Extra modifiers are a different chord (Cmd+Shift+4 screenshots).
        assert!(!jump_hints_visible(&keymap, true, false, true));
        assert!(!jump_hints_visible(&keymap, true, true, false));
        // Nothing held, nothing shown.
        assert!(!jump_hints_visible(&keymap, false, false, false));
        // Alt alone is not a jump modifier by default.
        assert!(!jump_hints_visible(&keymap, false, true, false));

        // Rebinding moves the trigger with it.
        let mut rebound = KeymapConfig::default();
        for slot in 0..JUMP_SLOTS {
            rebound.set(ShortcutId::JumpSession(slot), format!("mod-alt-{slot}"));
        }
        assert!(!jump_hints_visible(&rebound, true, false, false));
        assert!(jump_hints_visible(&rebound, true, true, false));

        // A jump combo with no modifiers must not pin the overlay open.
        let mut bare = KeymapConfig::default();
        bare.set(ShortcutId::JumpSession(0), "f5".into());
        assert!(!jump_hints_visible(&bare, false, false, false));
    }

    #[test]
    fn badge_combos_use_mac_glyphs_and_linux_text() {
        // macOS: glyphs in canonical ⌃⌥⇧⌘ order, no separators — the model
        // picker's ⌘N chip form.
        assert_eq!(badge_combo_on(true, "mod-2"), "⌘2");
        assert_eq!(badge_combo_on(true, "mod-shift-a"), "⇧⌘A");
        assert_eq!(badge_combo_on(true, "mod-alt-3"), "⌥⌘3");
        // A literal ctrl segment (macOS recorder spelling) is ⌃.
        assert_eq!(badge_combo_on(true, "ctrl-tab"), "⌃Tab");
        // Elsewhere the textual form stands.
        assert_eq!(badge_combo_on(false, "mod-2"), "Ctrl+2");
        assert_eq!(badge_combo_on(false, "mod-shift-a"), "Ctrl+Shift+A");
    }

    #[test]
    fn combo_modifiers_reads_the_stored_form() {
        assert_eq!(combo_modifiers("mod-1"), (true, false, false));
        assert_eq!(combo_modifiers("mod-alt-shift-k"), (true, true, true));
        assert_eq!(combo_modifiers("f5"), (false, false, false));
        assert_eq!(combo_modifiers("shift-tab"), (false, false, true));
    }

    #[test]
    fn a_keymap_missing_newer_shortcuts_keeps_its_customizations() {
        // Upgrade path: a file from a build that predates session cycling and
        // archiving carries the user's rebinds and defaults only the new rows.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"keymap": {"toggleSidebar": "mod-shift-x"}}"#,
        )
        .unwrap();
        let keymap = UiSettings::load(dir.path()).keymap;
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        assert_eq!(keymap.get(ShortcutId::ToggleTerminal), "mod-j");
        assert_eq!(
            keymap.get(ShortcutId::NextSession),
            ShortcutId::NextSession.default_combo()
        );
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-a");
    }

    #[test]
    fn terminal_height_clamps_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 5}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_MIN_HEIGHT
        );
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 99999}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_ABS_MAX_HEIGHT
        );
    }
}
