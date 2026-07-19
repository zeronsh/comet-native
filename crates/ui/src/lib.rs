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

pub mod composer;
pub mod loaders;
pub mod markdown;
pub mod motion;
pub mod settings;
pub mod shell;
pub mod state;
pub mod theme;
pub mod transcript;

use std::path::PathBuf;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

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

/// Run the headed app: tokio bridge up, engine bootstrap kicked off (probe →
/// connect-or-embed), 1320×880 window (min 900×600) with [`shell::Shell`] as the
/// root view, boot splash overlaid until the engine reports ready.
pub fn run_app(config: UiConfig) {
    gpui_platform::application().run(move |cx: &mut App| {
        // NB: pinned-rev API — `gpui_tokio::init(cx)` free function (not `Tokio::init`).
        gpui_tokio::init(cx);
        cx.set_global(theme::Theme::dark());
        composer::init(cx);

        let state = cx.new(|_| state::AppState::new());
        state::AppState::bootstrap(state.clone(), config.boot(), cx);

        // Graceful teardown: an in-process engine drains live runs and flushes
        // doc snapshots before the process exits (remote engines outlive us).
        let quit_state = state.clone();
        cx.on_app_quit(move |cx| {
            let shutdown = quit_state.read(cx).engine().cloned().map(|handle| {
                gpui_tokio::Tokio::spawn(cx, async move { handle.shutdown().await })
            });
            async move {
                if let Some(task) = shutdown {
                    let _ = task.await;
                }
            }
        })
        .detach();

        // comet window geometry: 1320×880, min 900×600 (feature-inventory §1.1).
        let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(600.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("comet".into()),
                    ..Default::default()
                }),
                app_id: Some("comet".into()),
                ..Default::default()
            },
            {
                let boot = config.boot();
                move |_, cx| cx.new(|cx| shell::Shell::new(state, boot, cx))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
