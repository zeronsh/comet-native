# gpui Standalone Chat-App Build Guide (from zed-industries/zed)

Repo state: freshly cloned zed. `rust-toolchain.toml` pins `channel = "1.95.0"`; workspace `edition = "2024"`. gpui crate version is `0.2.2`.

Important architectural note: **recent zed split gpui into a core crate plus separate platform backend crates.** The `gpui` crate holds the framework (elements, App/Entity/Window, executor, styling); the actual OS windowing backends live in sibling crates and are wired together by **`gpui_platform`**. All the `examples/*.rs` now start the app via `gpui_platform::application()`, not a bare `gpui::Application::new()`. A standalone app should depend on **both `gpui` and `gpui_platform`** via git.

---

## 1. Using gpui as a dependency

### `crates/gpui/Cargo.toml`
- `default = ["font-kit", "wayland", "x11", "windows-manifest"]`
- Feature flags: `wayland`, `x11` (Linux), `screen-capture`, `windows-manifest`, `inspector`, `test-support`, `bench`, `leak-detection`, `input-latency-histogram`.
- Core deps (all platforms): `taffy = "=0.12.2"` (flexbox layout), `resvg`/`usvg` (SVG), `lyon` (paths), cosmic/ttf-parser text, `sum_tree`, `smallvec`, `futures`, `async-task`, `parking_lot`, `scheduler`, `refineable`, `accesskit`.
- macOS target deps: `cocoa`, `core-foundation`, `core-graphics = "0.24"`, `core-text = "21"`, `metal`, `objc`, plus the zed fork `font-kit = { git = "https://github.com/zed-industries/font-kit", rev = "94b0f28...", package = "zed-font-kit" }`.
- Windows: `windows = "0.61"`. Linux/BSD: `pathfinder_geometry`; wayland/x11 gated by features.

### `gpui_platform` (the entry point you actually call)
`crates/gpui_platform/src/gpui_platform.rs`:
```rust
pub fn application() -> gpui::Application {
    gpui::Application::with_platform(current_platform(false))
}
// current_platform() #[cfg]-selects gpui_macos::MacPlatform / gpui_linux / gpui_windows / gpui_web
```
Backend crates: `gpui_macos`, `gpui_linux`, `gpui_windows`, `gpui_web`, `gpui_wgpu`. `gpui_platform/Cargo.toml` exposes features `wayland`, `x11`, `font-kit`, `screen-capture` that forward to the backend crates.

### The `gpui::Application` API
`crates/gpui/src/app.rs`:
- `pub struct Application(Rc<AppCell>)`; `Application::with_platform(...)`, `.run(|cx: &mut App| { ... })` (line 225), and `.run_embedded(...) -> ApplicationHandle` (line 246) for host-driven event loops.

### Canonical hello-world
`crates/gpui/examples/hello_world.rs`:
```rust
use gpui::{App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size};
use gpui_platform::application;

struct HelloWorld { text: SharedString }

impl Render for HelloWorld {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().bg(rgb(0x505050)).size(px(500.)).child(format!("Hello, {}!", self.text))
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(500.), px(500.)), cx);
        cx.open_window(WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            |_, cx| cx.new(|_| HelloWorld { text: "World".into() })).unwrap();
        cx.activate(true);
    });
}
```
Model: `App` (root cx) → `cx.open_window(opts, |window, cx| cx.new(|cx| RootView{..}))`. State lives in `Entity<T>` created via `cx.new`; each renders through `impl Render for T { fn render(&mut self, &mut Window, &mut Context<Self>) -> impl IntoElement }`. `prelude::*` brings in the builder traits (`Styled`, `ParentElement`, `InteractiveElement`, `StatefulInteractiveElement`, `IntoElement`, `Render`).

### The examples directory (`crates/gpui/examples/`)
Most relevant:
- **`hello_world.rs`** – window + basic styled `div`s.
- **`input.rs`** – full hand-rolled text input (see §5), 778 lines. The canonical text-input reference.
- **`uniform_list.rs`** – fixed-height virtualized list via `uniform_list(...)`.
- **`list_example.rs`** – variable-height `list()` + `ListState` with `ListAlignment::Bottom` (chat-style), manual scrollbar math.
- **`animation.rs`** – `with_animation` + `Animation::new().repeat().with_easing(bounce(ease_in_out))` rotating an SVG.
- **`scrollable.rs`, `opacity.rs`, `gradient.rs`, `shadow.rs`, `window_shadow.rs`, `svg/`, `text.rs`, `text_wrapper.rs`, `text_layout.rs`** – styling/text primitives.
- **`popover.rs`, `tree.rs`, `grid_layout.rs`, `data_table.rs`, `drag_drop.rs`, `tab_stop.rs`, `focus_visible.rs`, `a11y.rs`** – interaction/layout/focus.
- **`view_example/`, `move_entity_between_windows.rs`, `ownership_post.rs`** – Entity/ownership model demos.
- **`image*`, `gif_viewer.rs`, `pattern.rs`, `mouse_pressure.rs`, `set_menus.rs`, `system_notifications.rs`, `window_movable.rs`, `window_positioning.rs`, `layer_shell.rs`** – platform features.

---

## 2. Virtualized lists

Two elements, both in `crates/gpui`:

### `uniform_list` — fixed row height
Signature used in `examples/uniform_list.rs`:
```rust
uniform_list("entries", 50, cx.processor(|_this, range: Range<usize>, _window, _cx| {
    range.map(|ix| div().id(ix).child(format!("Item {ix}"))).collect::<Vec<_>>()
})).h_full()
```
`cx.processor(...)` gives the closure access to `&mut Self` and only renders the visible `range`. Cheapest option when every row is the same height.

### `list` + `ListState` — variable height (chat scrollback)
`crates/gpui/src/elements/list.rs`:
- `pub fn list(state: ListState, render: impl Fn(usize, &mut Window, &mut App) -> AnyElement)`; builder `.with_sizing_behavior(ListSizingBehavior::{Auto|Infer})`, `.with_horizontal_sizing_behavior(...)`.
- `ListState::new(item_count, alignment, overdraw_px)` — `ListAlignment::Bottom` means "scroll bottom-to-top like a chat log" (line 164-169); `ListAlignment::Top` for normal lists. `overdraw` is how far past the viewport to render.
- Item measurement: heights are measured lazily. `.measure_all()` / `with_uniform_item_height(px)` / `ListMeasuringBehavior::{Measure, Visible(default)}`. Heights stored in a `sum_tree` so scroll offset ↔ item index conversions are O(log n).
- Mutation API (must be called when item count/heights change): `splice(old_range, count)`, `splice_focusable(...)`, `reset(count)`, `reset_with_uniform_height(count, h)`, `remeasure()`, `remeasure_items(range)`.
- Scroll/state API: `scroll_to_end()`, `scroll_by(px)`, `scroll_to_reveal_item(ix)`, `logical_scroll_top() -> ListOffset { item_ix, offset_in_item }`, `is_scrolled_to_end() -> Option<bool>`, `item_count()`, `set_scroll_handler(...)`, plus scrollbar helpers `max_offset_for_scrollbar()`, `scroll_px_offset_for_scrollbar()`, `viewport_bounds()`.
- **Sticky-to-bottom / follow-tail**: with `ListAlignment::Bottom`, the list stays pinned to the newest item; `ListScrollEvent` exposes `is_following_tail: bool`. This is the mechanism a chat log needs.

### How zed's agent chat uses it
`crates/agent_ui/src/conversation_view/thread_view.rs` (12.6k lines):
- Holds `list_state: ListState` on the view struct (line ~590).
- Renders (~line 6019):
```rust
list(self.list_state.clone(),
    cx.processor(move |this, index: usize, window, cx| {
        let entries = this.thread.read(cx).entries();
        if let Some(entry) = entries.get(index) {
            centered_container(this.render_entry(index, entries.len(), entry, window, cx).into_any_element()).into_any_element()
        } else if this.generating_indicator_in_list {
            this.render_generating(..).into_any_element()
        } else { Empty.into_any() }
    }))
.with_sizing_behavior(gpui::ListSizingBehavior::Auto)
```
- Autoscroll on new content: `self.list_state.scroll_to_end()` (lines 1692, 6910); a `scroll_to_end(&mut self, cx)` helper wraps it (line 6909). It reads `list_state.logical_scroll_top().item_ix` to decide "is at top/bottom" (lines 1060, 7003, 7448). `threads_archive_view.rs` also uses `ListState::new(0, ListAlignment::Top, px(1000.))`.

---

## 3. Markdown rendering (`crates/markdown`)

`crates/markdown/src/` — files: `markdown.rs`, `parser.rs`, `selection.rs`, `path_range.rs`, `mermaid.rs`, `html.rs`. Deps: `pulldown-cmark` (parse), `linkify` (bare links), `html5ever`/`markup5ever_rcdom` (HTML), `language` (tree-sitter syntax highlighting), `theme`, `ui`.

### The `Markdown` entity (streaming-first design)
`crates/markdown/src/markdown.rs`:
- `Markdown::new(source, style, language_registry, cx)` / `new_with_options(...)` / `new_text(...)` (links-only, cheap).
- Streaming API — this is exactly the agent-response case:
  - `append(&mut self, text: &str, cx)` (line 733): `self.source = source + text; self.parse(cx);`
  - `replace(source, cx)` (738), `reset(source, cx)` (774) — reset keeps old parsed content visible until the new parse lands ("Don't clear parsed_markdown here").
- **Incremental / async parsing**: `parse()` (line 928) runs on a background thread via `cx.background_spawn(...)` and stores `pending_parse: Option<Task<()>>`. If a parse is in-flight when new text arrives, it sets `should_reparse = true` and re-kicks when the current one finishes (coalescing) — so streaming tokens don't block the UI. `is_parsing()` exposes state.
- `MarkdownStyle` (line 96) with `themed(font, window, cx)` and `themed_with_overrides(...)` pulls `cx.theme().syntax()` (a `SyntaxTheme`) for code highlighting; `HeadingLevelStyles`, `BlockQuoteKindColors`, `with_buffer_font`, `with_muted_text` helpers.
- Rendering: `MarkdownElement` (line 1244, `impl Element` at 2065) walks `ParsedMarkdown` events. Code blocks: `CodeBlockRenderer::Default { border, .. }` or a custom `render`/`transform` closure (`CodeBlockRenderFn`, `CodeBlockTransformFn`). Language resolution via `languages_by_name` / `languages_by_path` (`TreeMap<SharedString, Arc<Language>>`) → tree-sitter highlight runs. Handles links, footnotes, images, mermaid diagrams, and text selection (`selection.rs`).

### How agent_ui renders streaming assistant messages
`thread_view.rs` builds `MarkdownElement::new(entity, style)` per message; the assistant's `Entity<Markdown>` receives `.append(delta)` as tokens stream in, and each `cx.notify()` re-renders that message inside the virtualized `list`. There's also a custom code-block path (`parse_single_fenced_code_block`, `highlight_code_runs`, lines 258-460).

---

## 4. Animation

gpui exports (see `examples/animation.rs`): `Animation`, `AnimationExt` (extension trait, `use gpui::AnimationExt as _`), `Transformation`, easing fns `ease_in_out`, `bounce(...)`, `percentage(delta)`.

Canonical pattern:
```rust
svg().path(ARROW_CIRCLE_SVG)
    .with_animation("image_circle",
        Animation::new(Duration::from_secs(2)).repeat().with_easing(bounce(ease_in_out)),
        |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))))
```
`with_animation(id, Animation, |element, delta: f32| element)` drives a 0→1 `delta` over the duration; the closure maps it onto any element property. `Animation` builder: `.repeat()`, `.with_easing(...)`. Also `.with_transformation` for transforms, opacity animation (`examples/opacity.rs`). ui-crate wrapper: `ui::prelude::*` re-exports `traits::animation_ext::*` for spinners/pulses. Zed uses this for the agent "generating" spinner and panel slide-ins.

---

## 5. Text input

**gpui does NOT ship a ready-made text field.**

1. **The `examples/input.rs` hand-rolled `TextInput`** (778 lines) is the reference implementation and is copy-pasteable. It implements `EntityInputHandler`/`ElementInputHandler` (IME/marked-range), `FocusHandle`/`Focusable`, selection (`selected_range`, `selection_reversed`), a custom `Element` that shapes text with `ShapedLine`, mouse selection, and `actions!` for Backspace/Delete/arrows/Home/End/SelectAll/Copy/Cut/Paste/ShowCharacterPalette. This is the base for a chat composer without zed's editor.
2. **Zed's real composer uses the full `editor::Editor` crate** — effectively not extractable (deep deps on `language`, `project`, `multi_buffer`). `crates/ui_input` (`InputField`) wraps `Editor` — also not standalone.

Alternative: community **`gpui-component`** crate (longbridge/gpui-component) provides polished `Input`/`TextInput`, buttons, lists, etc.

---

## 6. Theming / styling

- **`Styled` trait** (`crates/gpui/src/styled.rs:22`): `.flex()`, `.flex_col()`, `.bg(color)`, `.p_2()`, `.gap_3()`, `.size(px(..))`, `.rounded_md()`, `.border_1()`, `.text_xl()`, `.text_color(impl Into<Hsla>)`, `.shadow_lg()`, `.overflow_y_scroll()`. Units: `px(f32)`, `rems(...)`, `relative(...)`, `percentage(...)`. Colors: `rgb(0x..)`, `rgba(..)`, `hsla(..)`, `Hsla::opacity(f)`.
- Interaction traits: `InteractiveElement` (`.on_click`, `.on_mouse_down`), `StatefulInteractiveElement` (requires `.id(...)`), `ParentElement` (`.child`/`.children`).
- **`crates/ui`** is zed's component library (**GPL-3.0** — licensing caveat). Components: button, label, icon, list, context_menu, popover, dropdown_menu, modal, tooltip, avatar, scrollbar, indicator, progress, tab/tab_bar, banner, callout, chip, disclosure, divider, data_table, keybinding, etc.
- **`crates/theme`**: `trait ActiveTheme { fn theme(&self) -> &Arc<Theme> }` on `App` → `cx.theme().colors()` / `cx.theme().syntax()`.

---

## 7. Async

`crates/gpui/src/executor.rs`:
- `BackgroundExecutor` (thread pool) and `ForegroundExecutor` (main thread) wrap gpui's own `scheduler` — **not smol/tokio directly**. `async-task` + `waker-fn` + `parking` under the hood.
- APIs: `cx.background_spawn(future) -> Task<R>` (off-thread, `Send`), `cx.spawn(async move |this, cx| {..})` (foreground, `Entity`-aware). `Task<R>` is the join handle (dropping cancels).
- **Tokio interop**: `crates/gpui_tokio` — `Tokio::init(cx)` builds a multi-thread tokio runtime as a global; `Tokio::spawn(cx, future) -> Task<Result<R, JoinError>>` runs a tokio future as a gpui `Task` (cancelled if dropped). For network streams: `Tokio::spawn` the future, hop back with `cx.spawn` to `entity.update` + append deltas.

---

## 8. Versions to pin in the new workspace

- `rust-toolchain.toml`: **`channel = "1.95.0"`**, `edition = "2024"`.
- gpui crate version **`0.2.2`** (also on crates.io, homepage gpui.rs) — for a git dependency use the same commit for `gpui`, `gpui_platform`, and backend crates:
  ```toml
  gpui = { git = "https://github.com/zed-industries/zed", rev = "<pin>" }
  gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "<pin>" }
  ```
- macOS transitive git fork: `zed-font-kit` (rev `94b0f28...`).
- **Licensing**: `crates/markdown`, `crates/ui`, `crates/theme`, `crates/editor` are **GPL-3.0**; gpui is Apache-2.0. For a permissive app: reimplement those (pulldown-cmark directly) or accept GPL.

### Minimal dependency set for a chat app
Apache-2.0 only: `gpui` + `gpui_platform` → `hello_world.rs` shell, `list` + `ListState(Bottom)` for scrollback (§2), `examples/input.rs` TextInput for composer (§5), `with_animation` for spinners (§4), `gpui_tokio` for API calls (§7), hand-rolled markdown via `pulldown-cmark`. If GPL acceptable: add `markdown`, `ui`, `theme` for faster polish.
