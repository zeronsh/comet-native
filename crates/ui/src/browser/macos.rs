//! AppKit boundary for the browser. Wry owns the native child and its UI
//! delegate; our navigation delegate supplies browser policy and state. All
//! callbacks enqueue events, never re-enter GPUI. No page-to-engine IPC.
use super::model::{PageState, Presentation, allowed_navigation};
use gpui::{Bounds, Pixels, Window};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSEventMask, NSEventModifierFlags, NSImage,
    NSView,
};
use objc2_foundation::{
    NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions, NSNumber, NSObject,
    NSObjectNSKeyValueObserverRegistration, NSObjectProtocol, NSString,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKNavigationResponse, WKNavigationResponsePolicy, WKSnapshotConfiguration, WKWebView,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
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
    Snapshot,
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

pub(super) struct NativePage(Rc<RefCell<Host>>);

pub(super) struct Host {
    web: WebView,
    view: Retained<WKWebView>,
    observer: Retained<Observer>,
    monitor: Option<Retained<AnyObject>>,
    shortcuts: Rc<RefCell<Vec<String>>>,
    bounds: Option<Bounds<Pixels>>,
    presentation: Presentation,
    snapshot: Rc<RefCell<Option<Arc<gpui::Image>>>>,
    snapshot_epoch: Rc<Cell<u64>>,
    tx: Sender,
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
            monitor,
            shortcuts,
            bounds: None,
            presentation: Presentation::Hidden,
            snapshot: Rc::new(RefCell::new(None)),
            snapshot_epoch: Rc::new(Cell::new(0)),
            tx,
        }))))
    }
    pub fn handle(&self) -> Rc<RefCell<Host>> {
        self.0.clone()
    }
    pub fn snapshot(&self) -> Option<Arc<gpui::Image>> {
        self.0.borrow().snapshot.borrow().clone()
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
        let _ = host.web.evaluate_script_with_callback(
            "(() => { const link = document.querySelector('link[rel~=icon]'); return link ? link.href : new URL('/favicon.ico', location.href).href; })()",
            move |value| {
                if let Ok(url) = serde_json::from_str::<String>(&value) {
                    let _ = tx.try_send(NativeEvent::Favicon { page: page.clone(), url });
                }
            },
        );
    }
}

fn has_focus(view: &NSView) -> bool {
    view.window()
        .and_then(|w| w.firstResponder())
        .and_then(|r| r.downcast::<NSView>().ok())
        .is_some_and(|v| v.isDescendantOf(view))
}

impl Host {
    pub fn sync(&mut self, bounds: Bounds<Pixels>, _scale: f32) {
        if self.bounds != Some(bounds) {
            self.bounds = Some(bounds);
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
        }
        self.update_visibility();
    }
    fn update_visibility(&self) {
        let visible = self.presentation == Presentation::Live
            && self.observer.ivars().error.borrow().is_none()
            && self
                .bounds
                .is_some_and(|b| f32::from(b.size.width) > 1.0 && f32::from(b.size.height) > 1.0);
        if !visible && has_focus(&self.view) {
            let _ = self.web.focus_parent();
        }
        if self.view.isHidden() == visible {
            let _ = self.web.set_visible(visible);
        }
    }
    fn present(&mut self, presentation: Presentation) {
        if self.presentation != presentation {
            self.snapshot_epoch
                .set(self.snapshot_epoch.get().wrapping_add(1));
            if presentation == Presentation::Covered && self.presentation == Presentation::Live {
                self.capture();
            } else {
                self.snapshot.borrow_mut().take();
            }
            self.presentation = presentation;
        }
        self.update_visibility();
    }
    fn capture(&self) {
        let Some(bounds) = self.bounds else {
            return;
        };
        if self.view.isHidden() {
            return;
        }
        let epoch = self.snapshot_epoch.get();
        let generation = self.snapshot_epoch.clone();
        let output = self.snapshot.clone();
        let tx = self.tx.clone();
        let config = unsafe { WKSnapshotConfiguration::new(self.view.mtm()) };
        // Limit both dimensions (and account for Retina's 2x output). A
        // snapshot is a temporary overlay aid, not a screenshot archive.
        let width = f64::from(f32::from(bounds.size.width));
        let height = f64::from(f32::from(bounds.size.height));
        let thumbnail_width = width.min(1024.0).min(1024.0 * width / height.max(1.0));
        unsafe {
            config.setSnapshotWidth(Some(&NSNumber::new_f64(thumbnail_width)));
        }
        let completion = block2::RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            if generation.get() != epoch || !error.is_null() {
                return;
            }
            let Some(image) = (unsafe { image.as_ref() }) else {
                return;
            };
            let Some(data) = image.TIFFRepresentation() else {
                return;
            };
            let Some(bitmap) = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
            else {
                return;
            };
            let png = unsafe {
                bitmap.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            };
            if let Some(png) = png.filter(|data| data.length() <= 16 * 1024 * 1024) {
                *output.borrow_mut() = Some(Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    png.to_vec(),
                )));
                let _ = tx.try_send(NativeEvent::Snapshot);
            }
        });
        unsafe {
            self.view
                .takeSnapshotWithConfiguration_completionHandler(Some(&config), &completion);
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.snapshot_epoch
            .set(self.snapshot_epoch.get().wrapping_add(1));
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
    }
}

#[cfg(feature = "browser-fixture")]
impl NativePage {
    pub fn fixture_visible(&self) -> bool {
        !self.0.borrow().view.isHidden()
    }
    pub fn fixture_eval(&self, script: &str) {
        self.0.borrow().web.evaluate_script(script).unwrap();
    }
}
