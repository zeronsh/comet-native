//! AppKit boundary for the browser. Wry owns the native child and its UI
//! delegate; our navigation delegate supplies browser policy and state. All
//! callbacks enqueue events, never re-enter GPUI. No page-to-engine IPC.
use super::model::{PageState, Presentation, allowed_navigation};
use gpui::{Bounds, Pixels, Window};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, msg_send,
};
use objc2_app_kit::{
    NSColor, NSEvent, NSEventMask, NSEventModifierFlags, NSView, NSWindowOrderingMode,
};
use objc2_foundation::{
    NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions, NSObject,
    NSObjectNSKeyValueObserverRegistration, NSObjectProtocol, NSString,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKNavigationResponse, WKNavigationResponsePolicy, WKWebView,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wry::{WebView, WebViewBuilderExtMacos, WebViewExtMacOS};

#[derive(Clone, Default)]
pub(super) struct BrowserData(Rc<RefCell<Option<Retained<objc2_web_kit::WKWebsiteDataStore>>>>);
impl BrowserData {
    fn configuration(
        &self,
        mtm: MainThreadMarker,
    ) -> Retained<objc2_web_kit::WKWebViewConfiguration> {
        let mut data = self.0.borrow_mut();
        let store = data.get_or_insert_with(|| unsafe {
            objc2_web_kit::WKWebsiteDataStore::nonPersistentDataStore(mtm)
        });
        let configuration = unsafe { objc2_web_kit::WKWebViewConfiguration::new(mtm) };
        unsafe {
            configuration.setWebsiteDataStore(store);
        }
        configuration
    }
}

type Sender = tokio::sync::mpsc::Sender<NativeEvent>;
const OBSERVED: [&str; 5] = ["URL", "title", "loading", "canGoBack", "canGoForward"];

pub(super) enum NativeEvent {
    Changed,
    Finished,
    NewTab(String),
    Key(gpui::Keystroke),
    Favicon { page: String, url: String },
}

struct ObserverState {
    tx: Sender,
    pending: Cell<bool>,
    error: RefCell<Option<String>>,
    requested_url: RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZeronBrowserObserver"]
    #[ivars = ObserverState]
    struct Observer;
    unsafe impl NSObjectProtocol for Observer {}
    impl Observer {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe(&self, _key: Option<&NSString>, _object: Option<&AnyObject>, _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>, _context: *mut std::ffi::c_void) {
            self.changed();
        }
    }
    unsafe impl WKNavigationDelegate for Observer {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn policy(&self, _view: &WKWebView, action: &WKNavigationAction, decision: &block2::Block<dyn Fn(WKNavigationActionPolicy)>) {
            let url = unsafe { action.request().URL() }.and_then(|u| u.absoluteString()).map(|u| u.to_string()).unwrap_or_default();
            if allowed_navigation(&url) && unsafe { action.targetFrame() }.is_some_and(|frame| unsafe { frame.isMainFrame() }) {
                *self.ivars().requested_url.borrow_mut() = Some(url.clone());
            }
            decision.call((if allowed_navigation(&url) { WKNavigationActionPolicy::Allow } else { WKNavigationActionPolicy::Cancel },));
        }
        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn response(&self, _view: &WKWebView, response: &WKNavigationResponse, decision: &block2::Block<dyn Fn(WKNavigationResponsePolicy)>) {
            let displayable = unsafe { response.canShowMIMEType() };
            if !displayable { self.fail("This file can’t be previewed here. Open it in your default browser."); }
            decision.call((if displayable { WKNavigationResponsePolicy::Allow } else { WKNavigationResponsePolicy::Cancel },));
        }
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn start(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.ivars().error.borrow_mut().take(); self.changed();
        }
        #[unsafe(method(webView:didCommitNavigation:))]
        fn commit(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.ivars().requested_url.borrow_mut().take(); self.changed();
        }
        #[unsafe(method(webView:didFinishNavigation:))]
        fn finish(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.changed(); let _ = self.ivars().tx.try_send(NativeEvent::Finished);
        }
        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn provisional_error(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            if error.code() != -999 { self.fail("Check the address and make sure your server is running, then try again."); }
        }
        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn navigation_error(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            if error.code() != -999 { self.fail("The connection was interrupted. Try loading this page again."); }
        }
        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn terminated(&self, _view: &WKWebView) {
            self.fail("The page stopped responding. Reload to continue.");
        }
    }
);

impl Observer {
    fn new(tx: Sender, mtm: MainThreadMarker) -> Retained<Self> {
        let object = mtm.alloc().set_ivars(ObserverState {
            tx,
            pending: Cell::new(false),
            error: RefCell::new(None),
            requested_url: RefCell::new(None),
        });
        unsafe { msg_send![super(object), init] }
    }
    fn changed(&self) {
        if !self.ivars().pending.replace(true) {
            if self.ivars().tx.try_send(NativeEvent::Changed).is_err() {
                self.ivars().pending.set(false);
            }
        }
    }
    fn fail(&self, message: &str) {
        *self.ivars().error.borrow_mut() = Some(message.into());
        self.changed();
    }
}

struct ClipState {
    dragging: Cell<bool>,
    region: Cell<objc2_foundation::NSRect>,
}

// Clips native content to GPUI's current paint mask. During app drags the
// entire native subtree passes pointer events back to GPUI, without hiding it.
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZeronBrowserClipView"]
    #[ivars = ClipState]
    struct BrowserClipView;
    impl BrowserClipView {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: objc2_foundation::NSPoint) -> *mut NSView {
            let region = self.ivars().region.get();
            if self.ivars().dragging.get()
                || point.x < region.origin.x || point.x >= region.origin.x + region.size.width
                || point.y < region.origin.y || point.y >= region.origin.y + region.size.height
            { std::ptr::null_mut() }
            else { unsafe { msg_send![super(self), hitTest: point] } }
        }
    }
);

pub(super) struct NativePage(Rc<RefCell<Host>>);

pub(super) struct Host {
    web: WebView,
    view: Retained<WKWebView>,
    observer: Retained<Observer>,
    clip: Retained<BrowserClipView>,
    clip_mask: Retained<AnyObject>,
    parent: Retained<NSView>,
    visible_bounds: Option<Bounds<Pixels>>,
    background_color: Option<Retained<NSColor>>,
    monitor: Option<Retained<AnyObject>>,
    shortcuts: Rc<RefCell<Vec<String>>>,
    bounds: Option<Bounds<Pixels>>,
    presentation: Presentation,
    tx: Sender,
    #[cfg(feature = "browser-fixture")]
    visibility_changes: Cell<u64>,
}

impl NativePage {
    pub fn new(window: &Window, data: &BrowserData, tx: Sender) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("Browser must be created on the main thread")?;
        let new_tab = tx.clone();
        let web = wry::WebViewBuilder::new()
            .with_webview_configuration(data.configuration(mtm))
            .with_visible(false)
            .with_focused(false)
            .with_incognito(true)
            .with_new_window_req_handler(move |url, _| {
                if allowed_navigation(&url) {
                    let _ = new_tab.try_send(NativeEvent::NewTab(url));
                }
                wry::NewWindowResponse::Deny
            })
            .with_download_started_handler(|_, _| false)
            .build_as_child(window)
            .map_err(|e| e.to_string())?;
        let view = Retained::into_super(web.webview());
        window
            .enable_scene_overlay()
            .map_err(|error| error.to_string())?;
        let parent = unsafe { view.superview() }.ok_or("Browser parent is missing")?;
        let clip: Retained<BrowserClipView> = unsafe {
            let object = mtm.alloc().set_ivars(ClipState {
                dragging: Cell::new(false),
                region: Cell::new(objc2_foundation::NSRect::ZERO),
            });
            msg_send![super(object), initWithFrame: objc2_foundation::NSRect::ZERO]
        };
        clip.setWantsLayer(true);
        clip.setAutoresizesSubviews(false);
        let clip_mask: Retained<AnyObject> = unsafe { msg_send![class!(CALayer), new] };
        unsafe {
            let color = NSColor::blackColor().CGColor();
            let _: () = msg_send![&*clip_mask, setBackgroundColor: &*color];
            let layer: *mut AnyObject = msg_send![&*clip, layer];
            let _: () = msg_send![layer, setMasksToBounds: true];
            let _: () = msg_send![layer, setMask: &*clip_mask];
        }
        clip.setHidden(true);
        parent.addSubview(&clip);
        // Keep every native tab beneath the shared GPUI overlay plane.
        for sibling in parent.subviews() {
            if sibling.class().name() == c"GPUIOverlayView" {
                parent.addSubview_positioned_relativeTo(
                    &clip,
                    NSWindowOrderingMode::Below,
                    Some(&sibling),
                );
                break;
            }
        }
        clip.addSubview(&view);

        let observer = Observer::new(tx.clone(), mtm);
        unsafe {
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*observer)));
            for key in OBSERVED {
                view.addObserver_forKeyPath_options_context(
                    &observer,
                    &NSString::from_str(key),
                    NSKeyValueObservingOptions::empty(),
                    std::ptr::null_mut(),
                );
            }
        }
        let shortcuts = Rc::new(RefCell::new(Vec::<String>::new()));
        let monitor_view = view.clone();
        let monitor_tx = tx.clone();
        let monitor_shortcuts = shortcuts.clone();
        let callback = block2::RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| {
            let e = unsafe { event.as_ref() };
            if monitor_view.isHidden()
                || !has_focus(&monitor_view)
                || e.window(mtm) != monitor_view.window()
            {
                return event.as_ptr();
            }
            let mods = e.modifierFlags();
            let key = e
                .charactersIgnoringModifiers()
                .map(|s| s.to_string().to_lowercase())
                .unwrap_or_default();
            let mut combo = String::new();
            if mods.contains(NSEventModifierFlags::Command) {
                combo.push_str("cmd-");
            }
            if mods.contains(NSEventModifierFlags::Control) {
                combo.push_str("ctrl-");
            }
            if mods.contains(NSEventModifierFlags::Option) {
                combo.push_str("alt-");
            }
            if mods.contains(NSEventModifierFlags::Shift) {
                combo.push_str("shift-");
            }
            combo.push_str(&key);
            let Ok(keystroke) = gpui::Keystroke::parse(&combo) else {
                return event.as_ptr();
            };
            let browser_key = matches!(
                combo.as_str(),
                "cmd-l" | "cmd-t" | "cmd-w" | "cmd-[" | "cmd-]" | "cmd-shift-r" | "cmd-k" | "cmd-,"
            );
            let app_key = monitor_shortcuts
                .borrow()
                .iter()
                .any(|s| gpui::Keystroke::parse(s).is_ok_and(|s| s == keystroke));
            if browser_key || app_key {
                if monitor_tx.try_send(NativeEvent::Key(keystroke)).is_ok() {
                    std::ptr::null_mut()
                } else {
                    event.as_ptr()
                }
            } else {
                event.as_ptr()
            }
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &callback)
        };
        Ok(Self(Rc::new(RefCell::new(Host {
            web,
            view,
            observer,
            clip,
            clip_mask,
            parent,
            visible_bounds: None,
            background_color: None,
            monitor,
            shortcuts,
            bounds: None,
            presentation: Presentation::Hidden,
            #[cfg(feature = "browser-fixture")]
            visibility_changes: Cell::new(0),
            tx,
        }))))
    }
    pub fn handle(&self) -> Rc<RefCell<Host>> {
        self.0.clone()
    }
    pub fn focus_chrome(&self) {
        let _ = self.0.borrow().web.focus_parent();
    }
    pub fn set_shortcuts(&self, shortcuts: Vec<String>) {
        *self.0.borrow().shortcuts.borrow_mut() = shortcuts;
    }
    pub fn present(&mut self, presentation: Presentation) {
        self.0.borrow_mut().present(presentation);
    }
    pub fn load(&self, url: &str) -> Result<(), String> {
        let host = self.0.borrow();
        host.observer.ivars().error.borrow_mut().take();
        *host.observer.ivars().requested_url.borrow_mut() = Some(url.into());
        host.web.load_url(url).map_err(|e| e.to_string())
    }
    pub fn reload(&self) {
        let host = self.0.borrow();
        host.observer.ivars().error.borrow_mut().take();
        // Retrying the requested URL also works after a failed provisional load.
        unsafe {
            host.view.reload();
        }
        host.observer.changed();
    }
    pub fn history(&self, forward: bool) {
        let host = self.0.borrow();
        unsafe {
            if forward {
                host.view.goForward();
            } else {
                host.view.goBack();
            }
        }
    }
    pub fn state(&self) -> PageState {
        let host = self.0.borrow();
        host.observer.ivars().pending.set(false);
        unsafe {
            PageState {
                url: host
                    .observer
                    .ivars()
                    .requested_url
                    .borrow()
                    .clone()
                    .or_else(|| {
                        host.view
                            .URL()
                            .and_then(|u| u.absoluteString())
                            .map(|u| u.to_string())
                    }),
                title: host.view.title().map(|s| s.to_string()).unwrap_or_default(),
                loading: host.view.isLoading(),
                can_back: host.view.canGoBack(),
                can_forward: host.view.canGoForward(),
                error: host.observer.ivars().error.borrow().clone(),
            }
        }
    }
    pub fn discover_favicon(&self, page: String) {
        let host = self.0.borrow();
        let tx = host.tx.clone();
        // Our navigation delegate owns readiness, so Wry's private pending-
        // script queue is intentionally bypassed. This runs only after finish.
        let completion = block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
            if !error.is_null() {
                return;
            }
            if let Some(url) =
                unsafe { value.as_ref() }.and_then(|value| value.downcast_ref::<NSString>())
            {
                let _ = tx.try_send(NativeEvent::Favicon {
                    page: page.clone(),
                    url: url.to_string(),
                });
            }
        });
        unsafe {
            host.view.evaluateJavaScript_completionHandler(
                &NSString::from_str("(() => { const link = document.querySelector('link[rel~=icon]'); return link ? link.href : new URL('/favicon.ico', location.href).href; })()"),
                Some(&completion),
            );
        }
    }
}

fn has_focus(view: &NSView) -> bool {
    view.window()
        .and_then(|w| w.firstResponder())
        .and_then(|r| r.downcast::<NSView>().ok())
        .is_some_and(|v| v.isDescendantOf(view))
}

impl Host {
    pub fn sync(&mut self, bounds: Bounds<Pixels>, mask: Bounds<Pixels>, dragging: bool) {
        // WebKit may fill newly exposed tiles a frame after a viewport change.
        // Match its page background beneath those tiles instead of exposing
        // the application's dark window background at the resize edge.
        unsafe {
            let color = self.view.underPageBackgroundColor();
            let unchanged = self
                .background_color
                .as_ref()
                .is_some_and(|previous| msg_send![&**previous, isEqual: &*color]);
            if !unchanged {
                // WebKit's native backing otherwise uses controlBackgroundColor,
                // independently of the CSS page background. Exposed resize tiles
                // must use the page color while the remote content catches up.
                let _: () = msg_send![&*self.view, _setBackgroundColor: &*color];
                let cg_color = color.CGColor();
                let layer: *mut AnyObject = msg_send![&*self.clip, layer];
                let _: () = msg_send![layer, setBackgroundColor: &*cg_color];
                self.background_color = Some(color);
            }
        }
        let visible = bounds.intersect(&mask);
        self.clip.ivars().dragging.set(dragging);
        if self.bounds != Some(bounds) || self.visible_bounds != Some(visible) {
            self.bounds = Some(bounds);
            self.visible_bounds = Some(visible);
            let x = f64::from(f32::from(visible.origin.x));
            let y = f64::from(f32::from(visible.origin.y));
            let width = f64::from(f32::from(visible.size.width)).max(0.);
            let height = f64::from(f32::from(visible.size.height)).max(0.);
            let y = if self.parent.isFlipped() {
                y
            } else {
                self.parent.bounds().size.height - y - height
            };
            let actions_disabled: bool = unsafe {
                let previous = msg_send![class!(CATransaction), disableActions];
                let _: () = msg_send![class!(CATransaction), setDisableActions: true];
                previous
            };
            // Keep the host stationary. Moving it while WebKit updates its
            // remote layer tree can combine the old origin with the new size.
            self.clip.setFrame(self.parent.bounds());
            let region = objc2_foundation::NSRect::new(
                objc2_foundation::NSPoint::new(x, y),
                objc2_foundation::NSSize::new(width, height),
            );
            self.clip.ivars().region.set(region);
            unsafe {
                let _: () = msg_send![&*self.clip_mask, setFrame: region];
            }
            let rect = wry::Rect {
                position: wry::dpi::LogicalPosition::new(
                    f64::from(f32::from(bounds.origin.x)),
                    f64::from(f32::from(bounds.origin.y)),
                )
                .into(),
                size: wry::dpi::LogicalSize::new(
                    f64::from(f32::from(bounds.size.width)),
                    f64::from(f32::from(bounds.size.height)),
                )
                .into(),
            };
            let _ = self.web.set_bounds(rect);
            unsafe {
                // Leave the implicit transaction open for the matching Metal
                // presentation. Committing here would expose native geometry early.
                let _: () = msg_send![class!(CATransaction), setDisableActions: actions_disabled];
            }
            #[cfg(feature = "browser-fixture")]
            unsafe {
                let layer: *mut AnyObject = msg_send![&*self.clip, layer];
                let frame: objc2_foundation::NSRect = msg_send![layer, frame];
                let presentation: *mut AnyObject = msg_send![layer, presentationLayer];
                let presented: objc2_foundation::NSRect = if presentation.is_null() {
                    frame
                } else {
                    msg_send![presentation, frame]
                };
                eprintln!(
                    "Browser geometry: bounds={bounds:?} mask={mask:?} clip={:?} layer={frame:?} presented={presented:?} web={:?} background={:?}",
                    self.clip.frame(),
                    self.view.frame(),
                    self.view.underPageBackgroundColor()
                );
            }
        }
        self.update_visibility();
    }
    fn update_visibility(&self) {
        let visible = self.presentation != Presentation::Hidden
            && self.observer.ivars().error.borrow().is_none()
            && self
                .visible_bounds
                .is_some_and(|b| f32::from(b.size.width) > 0. && f32::from(b.size.height) > 0.);
        if (!visible || self.presentation == Presentation::Passthrough) && has_focus(&self.view) {
            let _ = self.web.focus_parent();
        }
        if self.view.isHidden() == visible {
            #[cfg(feature = "browser-fixture")]
            self.visibility_changes
                .set(self.visibility_changes.get() + 1);
            let _ = self.web.set_visible(visible);
        }
        self.clip.setHidden(!visible);
    }
    fn present(&mut self, presentation: Presentation) {
        self.presentation = presentation;
        self.clip
            .ivars()
            .dragging
            .set(presentation == Presentation::Passthrough);
        self.update_visibility();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if has_focus(&self.view) {
            let _ = self.web.focus_parent();
        }
        unsafe {
            if let Some(monitor) = self.monitor.take() {
                NSEvent::removeMonitor(&monitor);
            }
            for key in OBSERVED {
                self.view
                    .removeObserver_forKeyPath(&self.observer, &NSString::from_str(key));
            }
            self.view.setNavigationDelegate(None);
            self.view.stopLoading();
        }
        self.view.removeFromSuperview();
        self.clip.removeFromSuperview();
    }
}

#[cfg(feature = "browser-fixture")]
impl NativePage {
    pub fn fixture_move_cursor(&self, x: f64, y: f64) {
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGWarpMouseCursorPosition(point: objc2_foundation::NSPoint) -> i32;
        }
        let host = self.0.borrow();
        let window = host.view.window().unwrap();
        let height = host.parent.clone().bounds().size.height;
        let p = window.convertPointToScreen(objc2_foundation::NSPoint::new(x, height - y));
        let screen = objc2_app_kit::NSScreen::mainScreen(host.view.mtm()).unwrap();
        unsafe {
            CGWarpMouseCursorPosition(objc2_foundation::NSPoint::new(
                p.x,
                screen.frame().size.height - p.y,
            ));
        }
    }
    pub fn fixture_backdrop_layers(&self) -> String {
        unsafe fn describe(layer: *mut AnyObject, depth: usize, out: &mut String) {
            if layer.is_null() || depth > 8 {
                return;
            }
            unsafe {
                let desc: Retained<NSString> = msg_send![layer, description];
                out.push_str(&format!("{}{}\n", " ".repeat(depth), desc));
                let layers: *mut AnyObject = msg_send![layer, sublayers];
                if !layers.is_null() {
                    let count: usize = msg_send![layers, count];
                    for i in 0..count.min(30) {
                        let child: *mut AnyObject = msg_send![layers, objectAtIndex:i];
                        describe(child, depth + 1, out);
                    }
                }
            }
        }
        let host = self.0.borrow();
        let mut out = String::new();
        for view in host.parent.subviews() {
            if view.class().name() == c"GPUIBackdropView" {
                unsafe {
                    let layer: *mut AnyObject = msg_send![&*view, layer];
                    describe(layer, 0, &mut out);
                }
            }
        }
        out
    }
    pub fn fixture_page_hit(&self, x: f64, y: f64) -> bool {
        let host = self.0.borrow();
        let point = objc2_foundation::NSPoint::new(x, host.parent.bounds().size.height - y);
        host.parent
            .hitTest(point)
            .is_some_and(|hit| hit.isDescendantOf(&host.clip))
    }
    pub fn fixture_backdrops(&self) -> Vec<(f64, f64, f64, f64)> {
        let host = self.0.borrow();
        host.parent
            .subviews()
            .iter()
            .filter(|view| view.class().name() == c"GPUIBackdropView" && !view.isHidden())
            .map(|view| {
                let r = view.frame();
                (
                    r.origin.x,
                    host.parent.bounds().size.height - r.origin.y - r.size.height,
                    r.size.width,
                    r.size.height,
                )
            })
            .collect()
    }
    pub fn fixture_geometry(&self) -> (f32, f32, f32) {
        let host = self.0.borrow();
        (
            host.bounds.unwrap().size.width.into(),
            host.view.frame().size.width as f32,
            host.visible_bounds.unwrap().size.width.into(),
        )
    }
    pub fn fixture_origin(&self) -> (f32, f32) {
        let b = self.0.borrow().bounds.unwrap();
        (b.origin.x.into(), b.origin.y.into())
    }
    pub fn fixture_overlay_visible(&self) -> bool {
        let host = self.0.borrow();
        host.parent
            .clone()
            .subviews()
            .iter()
            .any(|v| v.class().name() == c"GPUIOverlayView" && !v.isHidden())
    }
    pub fn fixture_visibility_changes(&self) -> u64 {
        let host = self.0.borrow();
        host.visibility_changes.get()
    }
    pub fn fixture_focus(&self) {
        let _ = self.0.borrow().web.focus();
    }
    pub fn fixture_focused(&self) -> bool {
        has_focus(&self.0.borrow().view)
    }
    pub fn fixture_overlay_at(&self, x: f64, y: f64) -> bool {
        let host = self.0.borrow();
        let parent = host.parent.clone();
        let point = objc2_foundation::NSPoint::new(
            x,
            if parent.isFlipped() {
                y
            } else {
                parent.bounds().size.height - y
            },
        );
        parent
            .hitTest(point)
            .is_some_and(|hit| hit.class().name() == c"GPUIOverlayView")
    }
    pub fn fixture_click(&self, x: f64, y: f64) {
        self.fixture_move_cursor(x, y);
        let host = self.0.borrow();
        let parent = host.parent.clone();
        let window = host.view.window().unwrap();
        let point = objc2_foundation::NSPoint::new(x, parent.bounds().size.height - y);
        let app = objc2_app_kit::NSApplication::sharedApplication(host.view.mtm());
        for kind in [
            objc2_app_kit::NSEventType::LeftMouseDown,
            objc2_app_kit::NSEventType::LeftMouseUp,
        ] {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(kind, point, NSEventModifierFlags::empty(), 0., window.windowNumber(), None, 0, 1, 1.).unwrap();
            app.postEvent_atStart(&event, false);
        }
    }
    pub fn fixture_visible(&self) -> bool {
        !self.0.borrow().view.isHidden()
    }
    pub fn fixture_eval(&self, script: &str) {
        unsafe {
            self.0
                .borrow()
                .view
                .evaluateJavaScript_completionHandler(&NSString::from_str(script), None);
        }
    }
}
