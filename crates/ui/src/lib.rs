//! comet-ui — the gpui viewport. Shell, sidebar, conversation, composer, terminal, diff pane.
//!
//! Design: ARCHITECTURE.md §4; animation catalog docs/research/feature-inventory.md §1.12;
//! virtualization/markdown techniques docs/research/mugen-pretext.md.

use gpui::{
    App, Application, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};

/// Root view — M3 replaces this placeholder with the app shell (sidebar + panel + panes).
struct Shell;

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(rgb(0x0a0a0a))
            .text_color(rgb(0xd4d4d4))
            .child("comet")
    }
}

pub fn run_app() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Shell),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}

fn application() -> Application {
    gpui_platform::application()
}
