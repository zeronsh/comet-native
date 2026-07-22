//! comet-ui — the gpui viewport. Shell, sidebar, conversation, composer, terminal,
//! diff pane.
//!
//! Design: ARCHITECTURE.md §4; animation catalog docs/research/feature-inventory.md
//! §1.12; virtualization/markdown techniques docs/research/mugen-pretext.md.
//!
//! M3a foundation:
//! - [`theme`] — always-dark monochrome theme (oklch-derived neutrals), a gpui Global;
//! - [`motion`] — the comet animation catalog over gpui `Animation` + cubic-bezier;
//! - [`state`] — `AppState` entity + `EngineHandle` (connect-or-embed engine);
//! - [`settings`] — persisted pane widths/collapse flags;
//! - [`shell`] — sidebar + main panel + right-pane scaffold + gate;
//! - [`loaders`] — comet pulse loader, gradient spinner, boot splash.

pub mod app_menus;
pub mod attachments;
pub mod changes;
pub mod composer;
pub mod icons;
pub mod loaders;
pub mod markdown;
pub mod motion;
pub mod pickers;
pub mod popover;
pub mod rail;
pub mod settings;
pub mod shell;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod transcript;

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

/// Embedded UI fonts — Geist and Geist Mono (variable), © Vercel Inc.,
/// licensed under the SIL Open Font License 1.1 (https://openfontlicense.org).
/// Bundled so the type ships with the binary instead of depending on what the
/// host system happens to have installed.
static FONT_GEIST: &[u8] = include_bytes!("../assets/fonts/Geist.ttf");
static FONT_GEIST_MONO: &[u8] = include_bytes!("../assets/fonts/GeistMono.ttf");
/// Static Geist weights alongside the variable file: gpui's cosmic-text path
/// (Linux) rasterizes variable fonts at their default instance only — it never
/// applies `wght` coordinates — so medium/semibold/bold text silently paints
/// at 400 with just the variable TTF registered. The statics give the face
/// matcher real 500/600/700 faces (macOS/CoreText applies the variable axis
/// natively and simply never falls through to these).
static FONT_GEIST_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Geist-Medium.ttf");
static FONT_GEIST_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Geist-SemiBold.ttf");
static FONT_GEIST_BOLD: &[u8] = include_bytes!("../assets/fonts/Geist-Bold.ttf");

/// Register the embedded fonts with the gpui text system. Failure is non-fatal:
/// the theme's system fallbacks take over (same families the CSS stack names).
fn register_fonts(cx: &App) {
    if let Err(err) = cx.text_system().add_fonts(vec![
        Cow::Borrowed(FONT_GEIST),
        Cow::Borrowed(FONT_GEIST_MONO),
        Cow::Borrowed(FONT_GEIST_MEDIUM),
        Cow::Borrowed(FONT_GEIST_SEMIBOLD),
        Cow::Borrowed(FONT_GEIST_BOLD),
    ]) {
        tracing::warn!(error = %err, "failed to register embedded Geist fonts");
    }
}

pub use comet_proto::HarnessId;
pub use state::EngineBootConfig;

/// Everything the headed binary passes in (config/env resolution lives in
/// `apps/comet`, not here).
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// Data directory — engine stores + `ui-settings.json`.
    pub data_dir: PathBuf,
    /// Localhost IPC port: connect if an engine daemon is listening, embed if not.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Edge bearer; `None` runs offline.
    pub edge_token: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
}

impl UiConfig {
    fn boot(&self) -> EngineBootConfig {
        EngineBootConfig {
            data_dir: self.data_dir.clone(),
            ipc_port: self.ipc_port,
            edge_url: self.edge_url.clone(),
            edge_token: self.edge_token.clone(),
            default_harness: self.default_harness,
        }
    }
}

/// What a dock-icon reopen needs to rebuild the main window after ⌘W closed it
/// (macOS keeps the process alive with just the menu bar, like zed).
struct ReopenState {
    state: gpui::Entity<state::AppState>,
    boot: EngineBootConfig,
}

impl gpui::Global for ReopenState {}

/// Run the headed app: tokio bridge up, engine bootstrap kicked off (probe →
/// connect-or-embed), 1320×880 window (min 900×600) with [`shell::Shell`] as the
/// root view, boot splash overlaid until the engine reports ready.
pub fn run_app(config: UiConfig) {
    let app = gpui_platform::application().with_assets(icons::Assets);
    // Dock-icon click with no window (⌘W closed it): rebuild the main window
    // around the still-running engine — zed does the same via `on_reopen`
    // (crates/zed/src/main.rs `app.on_reopen`).
    app.on_reopen(|cx| {
        if cx.windows().is_empty()
            && let Some(reopen) = cx.try_global::<ReopenState>()
        {
            let (state, boot) = (reopen.state.clone(), reopen.boot.clone());
            open_main_window(state, boot, cx);
        }
    });
    app.run(move |cx: &mut App| {
        // NB: pinned-rev API — `gpui_tokio::init(cx)` free function (not `Tokio::init`).
        gpui_tokio::init(cx);
        register_fonts(cx);
        cx.set_global(theme::Theme::dark());
        composer::init(cx);
        terminal::panel::init(cx);
        app_menus::init(cx);

        let state = cx.new(|_| state::AppState::new());
        state::AppState::bootstrap(state.clone(), config.boot(), cx);

        // Graceful teardown: an in-process engine drains live runs and flushes
        // doc snapshots before the process exits (remote engines outlive us).
        let quit_state = state.clone();
        cx.on_app_quit(move |cx| {
            let shutdown =
                quit_state.read(cx).engine().cloned().map(|handle| {
                    gpui_tokio::Tokio::spawn(cx, async move { handle.shutdown().await })
                });
            async move {
                if let Some(task) = shutdown {
                    let _ = task.await;
                }
            }
        })
        .detach();

        cx.set_global(ReopenState {
            state: state.clone(),
            boot: config.boot(),
        });
        open_main_window(state, config.boot(), cx);
        // Native menu bar — macOS gets the standard app menu (About/Services/
        // Hide/Quit ⌘Q), Edit clipboard verbs routed to the focused input, and
        // a Window menu (⌘M/⌘W). Without this, `NSApp.mainMenu` stays nil: no
        // Cmd+Q, and nothing for the system menu bar to show. Set after
        // `open_main_window` because `Shell::new` ran `apply_keymap`
        // synchronously, so `set_menus` reads the final bindings for the ⌘-key
        // equivalents (gpui snapshots the keymap at set time).
        cx.set_menus(app_menus::app_menus());
        cx.activate(true);
    });
}

/// Open the 1320×880 main window (min 900×600) with [`shell::Shell`] as the
/// root view. Called at boot and again from `on_reopen` if the dock icon is
/// clicked after ⌘W closed the window.
fn open_main_window(state: gpui::Entity<state::AppState>, boot: EngineBootConfig, cx: &mut App) {
    // comet window geometry: 1320×880, min 900×600 (feature-inventory §1.1).
    let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(600.))),
            // `kind` is deliberately left at its default `WindowKind::Normal`
            // (gpui platform.rs WindowOptions::default), which on macOS maps
            // to `NSNormalWindowLevel` (gpui_macos window.rs) — same as zed's
            // main window. Nothing here raises the window level or touches
            // presentation options; the "menu bar never appears" symptom came
            // from the missing `set_menus` call (nil `NSApp.mainMenu`), not
            // from window kind/level, and `appears_transparent` only affects
            // the titlebar, not the menu bar.
            // macOS: frameless-inset chrome like the original Electron app
            // (`titleBarStyle: "hiddenInset"`, traffic lights at 14,15 —
            // feature-inventory §1.1). No title text — the strip is
            // custom-drawn (zed sets `title: None` the same way). The
            // original is deliberately OPAQUE (no vibrancy), so
            // window_background stays default. On Linux/Windows
            // `appears_transparent` hides the system titlebar for our
            // custom-drawn chrome; harmless where unsupported.
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(14.), px(15.))),
            }),
            // Our own titlebar strip drags the window (WindowControlArea::
            // Drag + start_window_move) — mark the content view app-owned
            // so AppKit neither dead-zones the strip nor delays clicks.
            app_owns_titlebar_drag: true,
            app_id: Some("comet".into()),
            ..Default::default()
        },
        move |_, cx| cx.new(|cx| shell::Shell::new(state, boot, cx)),
    )
    .expect("failed to open window");
}
