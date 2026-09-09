# Native browser tabs for Zeron: system-webview v1

Research date: 2026-09-08. Status: implemented. All 746 UI tests and the real-shell macOS browser fixture pass; screenshots and provenance are in `docs/screenshots/browser`.

Zeron baseline: `8de07ee6d783a37c2cac39a259ba896d704c40e8` (latest fetched `origin/main`). This worktree was clean and fast-forwarded from `6b2ea31f`; the separate local `main` worktree was not modified.

Diri reference: [`c564784199cfbeabd29f011bac28467a2b12fccf`](https://github.com/cristicretu/diri/tree/c564784199cfbeabd29f011bac28467a2b12fccf). Findings below come from source inspection, not a running Diri build.

V1 uses the system webview and native compositing; CEF remains a possible later backend.

## Implementation

The implementation uses `browser/{mod,model,view,macos}.rs`, with Wry restricted
to macOS dependencies. The real shell owns session-specific browser entities;
blank tabs allocate no WebKit view. Existing tabs retain their page when hidden.
Closing a tab or deleting its session detaches the native view and removes its
observers and event monitor. A profile change clears the browser registry and
its shared nonpersistent website data store. Hiding a tab does **not** promise
to reclaim its WebKit memory; closing releases the host, while WebKit manages
its shared process caches.

Zui now renders deferred GPUI content on a transparent Metal view above
WKWebView, sharing the main renderer’s device and sprite atlas. Tooltips leave
the page live and preserve native focus and pointer input. Interactive overlays
intercept input until dismissed. Every newly created native browser view is
placed beneath this overlay plane.

A native clipping view follows GPUI's paint mask while the live WebView retains
its layout viewport. Pane drags update that viewport so the page reflows; pane
animations clip it without stretching a frozen image. The clipping view passes
pointer events back to GPUI during app drags. Closing animations retain page
content until the pane finishes closing. No snapshot handoff is used.

Native within-window backdrop effects sit below GPUI overlay content so frosted
menus blur the live browser pixels. Their bounds and rounded corners follow the
GPUI blur regions; opaque appearance and menu dismissal remove the effects.

The browser accepts HTTP(S) addresses only and has no engine IPC bridge.
Downloads and non-displayable responses are unsupported in the preview; users
can open the current address externally. Browser tabs are not restored after
restart. Linux uses the same chrome with an explicit external-browser message.
Localhost addresses always refer to the UI device, with a hint for remote chats.

`browser-fixture` runs the actual shell with isolated synthetic chat data and a
loopback HTML server. It captures the empty state, live native page, an open menu,
light appearance and a load failure. Native checks cover DOM navigation,
back/forward availability, SPA URL/title updates, independent tabs, shared
nonpersistent cookies, rapid hover, tooltip focus and hit testing, menu outside-click
isolation and input restoration, visibility, resizing/takeover and
teardown. It is gated behind an opt-in feature and is excluded from app builds.

```sh
cargo test --locked -p zeron-ui --lib -- --test-threads=1
cargo run --release --locked -p zeron-ui --example browser-fixture \
  --features browser-fixture -- /tmp/browser-captures
```

Run the fixture in a logged-in macOS desktop session for native validation and
screenshots. On Linux it needs an X11 session plus `xdotool` and ImageMagick; its
screenshots explicitly show the external-only fallback. The macOS CI job uploads
its captures as `browser-macos-captures`. These automated checks do not establish
clipboard/IME compatibility across websites or provide an idle-memory benchmark.

The sections below preserve the source findings and original implementation
plan; this section records the choices actually made for v1.

## Recommendation

Add `Browser(id)` to Zeron's existing right-pane surfaces. Build the address bar and navigation controls in GPUI, and embed a lazily created `WKWebView` through Wry for page content on macOS. Reuse the existing tab strip, per-session ownership, resize seam, and expand control. On Linux, initially offer an explicitly labeled external-browser fallback, matching Diri; embedded Linux browsing is a separate milestone.

The first release supports multiple independent browser tabs, editable URLs, back/forward/reload, page titles and favicons, open externally, and load-failure recovery. Keep pages alive when switching tabs or sessions during the app run; closing a browser tab destroys its native view. Browser tabs remain device-local and in memory, consistent with the existing panel lifecycle. No restart restoration in this milestone.

## Native compositing architecture

Use Wry/WKWebView on macOS, with a transparent GPUI layer above native page content. Deferred menus, dialogs and tooltips render on that layer while the page stays live. Native callbacks schedule foreground updates rather than re-entering GPUI entities during native callbacks.

The renderer adapts Apache-2.0 GPUI code from [`egoist/zed` at `57bd4fe`](https://github.com/egoist/zed/tree/57bd4fe181639797d395978d5de17bc9e10a6219/crates/gpui_macos). Preserve its license and source attribution in the dependency notices.

Keep the backend boundary small: create/navigate/history/reload, geometry and visibility, focus, close, and state events. Isolate Wry/WebKit handles in the platform module. This preserves the tab UI and model if CEF is evaluated later, but does not promise a drop-in replacement: CEF would still need its own process lifecycle, compositor/input integration, packaging and profiling. Do not add CEF dependencies or build a speculative multi-engine framework for v1.

## How Diri implements it

| Concern | Implementation and source |
| --- | --- |
| Sidebar chrome | [`inspector.rs`](https://github.com/cristicretu/diri/blob/c564784199cfbeabd29f011bac28467a2b12fccf/diri/crates/diri-app/src/inspector.rs): `WorkspaceSurface::Browser`, `BrowserAction`, `BrowserState`, editable query, and `render_browser`. Tabs store their browser query/state in per-session workspace collections. |
| Real page rendering | [`macos/browser.rs`](https://github.com/cristicretu/diri/blob/c564784199cfbeabd29f011bac28467a2b12fccf/diri/crates/diri-app/src/macos/browser.rs): `NativeBrowser` owns an active `BrowserPage` plus a map of inactive pages, each with its own native view/history. `WKWebView` is attached as an AppKit child through `raw-window-handle` and typed `objc2` bindings. |
| Geometry | `NativeBrowser::surface` uses a GPUI canvas paint callback to receive final body bounds. `BrowserPage::sync` converts top-left GPUI points to the parent NSView coordinate system and changes the frame only when needed. Blank pages allocate no native view. |
| State updates | WebKit navigation delegates and KVO observe URL, title, loading, and history availability. A bounded notification channel coalesces updates; the root projects current state into GPUI. Same-document navigation updates the address bar without polling. |
| Focus and input | Address-bar focus explicitly returns AppKit's first responder to GPUI. A native event monitor forwards selected browser/application shortcuts exactly once. A WKWebView subclass makes hit testing pass through during resize drags. |
| Overlays and animation | [`root.rs`](https://github.com/cristicretu/diri/blob/c564784199cfbeabd29f011bac28467a2b12fccf/diri/crates/diri-app/src/root.rs): `browser_visible` suppresses the native child beneath menus/dialogs and during incompatible pane geometry. Browser content bypasses the inspector's ordinary opacity/translation animation. Visibility restoration also handles cached GPUI paints. |
| URL behavior | Explicit HTTP(S) addresses are accepted; bare loopback hosts use HTTP and other bare hosts use HTTPS. Browser title/favicon state supplies tab labels. Favicon discovery resolves document links, limits downloaded/decoded data, and rejects stale results after navigation. |
| Cleanup and errors | Closing detaches the child, returns focus, removes event monitors/KVO, clears delegates, and stops loading. Navigation errors and WebKit process termination produce reloadable error UI. |
| Other platforms | `root.rs` sends navigation/open-external actions to `cx.open_url` outside macOS. It does not embed a Linux browser. |

Diri also has a [Playwright engine sidecar](https://github.com/cristicretu/diri/blob/c564784199cfbeabd29f011bac28467a2b12fccf/diri/crates/diri-engine/src/browser.rs) for `test.run` and `browser.act`. That is a separate browser system; the sidebar is not a streamed Playwright page. Porting that sidecar is unnecessary for this feature.

## Zeron integration points

| Existing code | Planned change |
| --- | --- |
| `crates/ui/src/shell.rs`: `RightSurface`, `SessionPanels`, `panel_key`, `right_tabs` | Add `Browser(u64)` and a browser registry owned by the shell. Use IDs unique across sessions within a window; record the owning panel key. Preserve the existing hidden-on-new-session-canvas behavior. |
| `right_surface_rows`, `resolved_right_active`, `set_right_active`, `close_right_surface` | Add browser title/icon/loading projection, selection, fallback after close, and deterministic native cleanup. Browser tabs are distinct instances, unlike the singleton Files surface. |
| `render_surface_picker`, `render_right_tab_strip` and its `+` menu | Add Browser entries and render page title/favicon with a generic fallback. Reuse tab reorder and close behavior. |
| `render_right_pane`, `right_pane_container`, resize/takeover handling | Render the browser surface and coordinate native visibility with actual bounds, clipping, drag state, route and overlays. |
| `crates/ui/src/shell/tabs.rs` | Preserve the shared titlebar, pane toggle and expand controls. |
| `crates/ui/src/surface_chrome.rs`, `icons.rs` | Reuse toolbar sizes, theme tokens and icon conventions. |
| `crates/ui/src/settings.rs`, `settings/shortcuts.rs` | Introduce browser action contexts and configurable shortcuts with explicit conflict handling. Do not revive legacy persisted pane-open fields. |
| `crates/ui/src/lib.rs`, `crates/ui/Cargo.toml` | Register a browser module; add target-gated WebKit/AppKit dependencies and URL parsing support. |

Suggested new modules: `browser/mod.rs` (surface/controller and events), `browser/model.rs` (state, IDs, URL normalization), `browser/view.rs` (GPUI chrome), `browser/platform/mod.rs` (small native-host interface), and `browser/platform/macos.rs` (WebKit ownership). Keep platform types out of `RightSurface` and shared state. Implement an external-only backend for unsupported platforms.

## Implementation sequence

### 1. Prove native embedding in Zeron's GPUI fork

Build an opt-in macOS fixture with one loopback page in a GPUI pane before changing the production shell. The pinned Zui source already implements `HasWindowHandle` for `Window`; invoke the trait explicitly where GPUI's own `window_handle()` name overlaps. Check the exact AppKit parent, main-thread ownership, logical coordinates and backing-scale behavior.

Use macOS-only Wry plus narrowly enabled `objc2`, `objc2-app-kit`, `objc2-foundation`, `objc2-web-kit` and `block2` dependencies. Verify coexistence with Zeron's existing `objc` dependency. Audit layered rendering against the pinned Zui renderer and its existing blur/edge-fade changes. Prototype a minimal native-surface/overlay seam in Zui if needed; preserve Zui's existing renderer customizations. Prefer live page content beneath correctly composited menus. If layered integration cannot be delivered in v1, use a temporary page snapshot during overlays, with input disabled and restoration tested.

Exit: a live page accepts input, reflows while resizing, hides/restores correctly, and detaches safely on window close. This is the main technical feasibility gate.

### 2. Add browser tabs and GPUI chrome

Implement shell creation/selection/reordering/closing and per-session ownership, initially usable with a fake backend. Add the toolbar, address input, empty state, loading indicator and error/retry state. Use an existing GPUI input component that supports selection, clipboard and IME; do not copy Diri's query editor wholesale.

Normalize URLs in one tested function: preserve explicit HTTP(S), default loopback addresses including IPv6 to HTTP, default ordinary hosts to HTTPS, and reject malformed or unsupported schemes with inline feedback. This is an address bar, not a search engine. Keep native navigation state separate from the address draft so redirects cannot overwrite text while the user edits.

Exit: multiple browser tabs behave consistently with existing surfaces, including switching sessions and closing a background tab.

### 3. Connect WebKit and lifecycle events

Create a WebKit page only on first navigation. Keep a native view per live tab so switching preserves DOM state, forms, scroll and history. Project state changes through a coalescing channel into the corresponding tab, including inactive tabs. Discard late callbacks after close/navigation using tab identity and generation checks.

Use Wry callbacks and controls where available; add a narrow native adapter for missing history/KVO, focus, snapshot or failure hooks. Native callbacks must enqueue foreground updates rather than re-entering GPUI during its update/paint. Add load/process failure recovery and bounded favicon loading. Choose an explicit web-data policy: for the initial preview feature, share a nonpersistent WebKit data store within the current workspace/profile, retaining logins across its live tabs but clearing them at app exit. Do not mix data between profile identities.

Apply the HTTP(S) policy to native navigations as well as address submissions. Support ordinary web apps' new-window links by routing HTTP(S) targets into a new Browser tab through Wry's new-window callback or a native delegate where needed. Leave downloads and broader browser permission UI to later work, with explicit unsupported behavior rather than silent failure. Do not expose a page-to-engine command bridge.

Exit: redirects, SPA history/title changes, errors and multi-tab navigation remain correct without polling or page reloads on selection.

### 4. Resolve native compositing and keyboard ownership

Centralize a browser visibility predicate in the shell. Require the chat route, ready app state, open pane, selected live Browser tab, and usable bounds. With a working overlay plane, leave the page visible beneath correctly composited overlays and route input to the overlay. Without that plane, temporarily show a bounded snapshot and suppress native input/content during covering overlays. Audit `render_overlays`, `overlay_owns_keyboard`, the right `+` menu, composer popovers, settings, dialogs and boot transitions; keyboard ownership alone is not a complete visual-occlusion test.

Zeron's `right_pane_container` keeps inner content at the larger endpoint width while clipping its animated outer width. A native child does not honor that GPUI clip. First test whether the new native-surface seam can apply the real visible clip during open/close/takeover tweens. Where native clipping cannot follow the tween correctly, animate a frozen page snapshot within GPUI and restore the same live page at settled bounds. Bound snapshot size/lifetime, discard stale captures, and use a neutral placeholder if capture fails; do not keep a snapshot-driven frame loop while browsing normally. During manual pane/window resizing, keep the page visible and update its frame from measured body bounds; pass pointer events through during app drags and restore hit testing on release/cancellation.

Coordinate AppKit responder changes with GPUI focus and Zeron's composer-focus restoration. Scope new-tab, close-tab, focus-address and history commands to browser focus; preserve ordinary web editing shortcuts. Zeron currently uses `mod-r` for Toggle right sidebar, so do not copy Diri's global reload binding. Preserve `mod-r` for the pane and start Reload with a toolbar button plus a configurable, conflict-checked `mod-shift-r` default. Forward the configured pane-toggle shortcut from WebKit too, so the pane remains closable while the page is focused.

Exit: no page paints over a dialog/chat/titlebar, no hidden page consumes keys, no shortcut dispatches twice, and reopening an overlay-hidden page works even when GPUI reuses cached paint.

### 5. Validate, document and release

Run pure Rust tests for URL normalization, browser/session ownership, stale events, close fallback, visibility decisions and shortcut conflicts. Use GPUI interaction tests for toolbar input and shell tab behavior. On macOS, exercise a loopback fixture with navigation, redirects, SPA changes, multiple tabs, clipboard/IME, new-window links, window resizing, pane dragging, takeover, overlays, loading failures and native teardown.

Check Linux compilation and explicitly labeled external-browser behavior. Profile idle CPU and memory while repeatedly opening/closing tabs and switching sessions; distinguish hiding from releasing a native page, and verify no accumulating views, monitors or observers. Run the repository's applicable formatting/build checks. Update `THIRD_PARTY_NOTICES.md` if Diri code is adapted, retaining its Apache-2.0 attribution and license notices.

Ship after the macOS fixture and real shell interactions pass. Source research on this Linux workspace does not validate AppKit behavior.

## Remote sessions and follow-up scope

The embedded browser runs on the UI machine. Opening `localhost:3000` while controlling a remote Zeron engine reaches the UI machine, not that engine. The initial UI should explain this when a loopback URL is used with a remote session and allow an explicitly entered reachable URL. Existing device-room command/file RPC is not an HTTP/WebSocket tunnel.

Follow-up work can add authenticated remote port forwarding, including HTTP/WebSocket upgrades and reconnect handling. It requires a separate engine/protocol/transport design. CEF should be reconsidered only when concrete needs justify it, such as Chromium-specific compatibility, a shared automation runtime, or platform requirements the system-webview path cannot satisfy. Compare installer size, cold browser startup, representative page memory and complete teardown before adopting it. A future Windows port can evaluate WebView2 composition independently. Embedded Linux browsing also needs a separate prototype for Zeron's X11 and Wayland hosts; Diri supplies no reusable Linux implementation. Restart tab restoration, developer tools, screenshots/attach-to-chat, console capture and agent control are later features rather than prerequisites for the sidebar browser.
