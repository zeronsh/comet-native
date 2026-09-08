//! Device-local browser tabs. GPUI owns chrome; the native host owns pages.
#[cfg(target_os = "macos")]
mod macos;
pub mod model;
mod view;

use crate::composer::{ComposerInput, ComposerInputEvent};
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Window,
};
use model::{PageState, Presentation};
gpui::actions!(
    browser,
    [Reload, FocusAddress, NewTab, CloseTab, Back, Forward]
);

pub(crate) fn bind_keys(cx: &mut App, keymap: &crate::settings::KeymapConfig) {
    use crate::settings::{ShortcutId, platform_combo};
    // Existing customized app chords win. In particular old settings may
    // already assign mod-shift-r to the right pane.
    let available = |combo: &str, own: Option<ShortcutId>| {
        !combo.is_empty()
            && !ShortcutId::ALL.iter().any(|id| {
                Some(*id) != own
                    && gpui::Keystroke::parse(&platform_combo(keymap.get(*id))).ok()
                        == gpui::Keystroke::parse(&platform_combo(combo)).ok()
            })
    };
    macro_rules! bind {
        ($combo:literal, $action:ident) => {
            if available($combo, None) {
                cx.bind_keys([gpui::KeyBinding::new(
                    &platform_combo($combo),
                    $action,
                    Some("Browser"),
                )]);
            }
        };
    }
    bind!("mod-l", FocusAddress);
    bind!("mod-t", NewTab);
    bind!("mod-w", CloseTab);
    bind!("mod-[", Back);
    bind!("mod-]", Forward);
    let reload = keymap.get(ShortcutId::BrowserReload);
    if available(reload, Some(ShortcutId::BrowserReload))
        && gpui::Keystroke::parse(&platform_combo(reload)).is_ok()
    {
        cx.bind_keys([gpui::KeyBinding::new(
            &platform_combo(reload),
            Reload,
            Some("Browser"),
        )]);
    }
}

#[derive(Clone, Debug)]
pub enum BrowserEvent {
    Changed,
    NewTab(Option<String>),
    Close,
}

/// A window/profile's ephemeral website data, allocated on first navigation.
#[derive(Clone, Default)]
pub struct BrowserContext {
    #[cfg(target_os = "macos")]
    data: macos::BrowserData,
}

pub struct BrowserSurface {
    #[cfg(target_os = "macos")]
    context: BrowserContext,
    address: Entity<ComposerInput>,
    focus: FocusHandle,
    pub page: PageState,
    pub favicon: Option<std::sync::Arc<gpui::Image>>,
    address_edited: bool,
    validation: Option<String>,
    remote: bool,
    pub(crate) chrome_hovered: bool,
    presentation: Presentation,
    _input_sub: Subscription,
    #[cfg(target_os = "macos")]
    native: Option<macos::NativePage>,
    #[cfg(target_os = "macos")]
    native_tx: tokio::sync::mpsc::Sender<macos::NativeEvent>,
    #[cfg(target_os = "macos")]
    _native_task: gpui::Task<()>,
    #[cfg(target_os = "macos")]
    favicon_task: Option<gpui::Task<()>>,
    #[cfg(target_os = "macos")]
    favicon_generation: u64,
}

impl EventEmitter<BrowserEvent> for BrowserSurface {}
impl Focusable for BrowserSurface {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl BrowserSurface {
    pub fn new(
        context: BrowserContext,
        remote: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address = cx.new(|cx| {
            ComposerInput::with_context("Website or localhost:3000", "PaletteSearch", cx)
                .with_text_metrics(12.0, 18.0)
                .with_accessibility_role(gpui::Role::TextInput)
        });
        let input_sub = cx.subscribe(&address, |this, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.address_edited =
                    this.address.read(cx).text() != this.page.url.as_deref().unwrap_or_default();
                this.validation = None;
                cx.notify();
            }
        });
        #[cfg(target_os = "macos")]
        let (native_tx, mut events) = tokio::sync::mpsc::channel(64);
        #[cfg(target_os = "macos")]
        let native_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.on_native_event(event, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        #[cfg(not(target_os = "macos"))]
        let _ = (window, context);
        Self {
            #[cfg(target_os = "macos")]
            context,
            address,
            focus: cx.focus_handle(),
            page: PageState::default(),
            favicon: None,
            address_edited: false,
            validation: None,
            remote,
            chrome_hovered: false,
            presentation: Presentation::Hidden,
            _input_sub: input_sub,
            #[cfg(target_os = "macos")]
            native: None,
            #[cfg(target_os = "macos")]
            native_tx,
            #[cfg(target_os = "macos")]
            _native_task: native_task,
            #[cfg(target_os = "macos")]
            favicon_task: None,
            #[cfg(target_os = "macos")]
            favicon_generation: 0,
        }
    }

    pub fn title(&self) -> gpui::SharedString {
        self.page.label().into()
    }
    pub fn set_remote(&mut self, remote: bool) {
        self.remote = remote;
    }
    pub fn set_shortcuts(&mut self, keymap: &crate::settings::KeymapConfig) {
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            native.set_shortcuts(
                crate::settings::ShortcutId::ALL
                    .iter()
                    .filter(|id| **id != crate::settings::ShortcutId::SaveFile)
                    .map(|id| crate::settings::platform_combo(keymap.get(*id)))
                    .collect(),
            );
        }
        #[cfg(not(target_os = "macos"))]
        let _ = keymap;
    }

    pub fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            native.focus_chrome();
        }
        window.focus(&self.address.focus_handle(cx), cx);
        window.dispatch_action(Box::new(crate::composer::SelectAll), cx);
    }

    pub fn set_presentation(&mut self, presentation: Presentation, cx: &mut Context<Self>) {
        if self.presentation == presentation {
            return;
        }
        #[cfg(target_os = "macos")]
        if presentation != Presentation::Covered {
            if let Some(image) = self.native.as_ref().and_then(|native| native.snapshot()) {
                cx.defer(move |cx| gpui::ImageSource::Image(image).evict(None, cx));
            }
        }
        self.presentation = presentation;
        #[cfg(target_os = "macos")]
        if let Some(native) = &mut self.native {
            native.present(presentation);
        }
        cx.notify();
    }

    pub fn navigate(&mut self, input: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = match model::normalize_address(input) {
            Ok(url) => url,
            Err(message) => {
                self.validation = Some(message.into());
                cx.notify();
                return;
            }
        };
        self.validation = None;
        self.address
            .update(cx, |input, cx| input.set_text(url.clone(), cx));
        self.address_edited = false;
        self.page.url = Some(url.clone());
        self.page.title.clear();
        self.page.error = None;
        self.clear_favicon(cx);
        #[cfg(target_os = "macos")]
        {
            self.favicon_generation += 1;
            self.favicon_task = None;
            let result = if let Some(native) = &self.native {
                native.load(&url)
            } else {
                macos::NativePage::new(window, &self.context.data, self.native_tx.clone())
                    .map(|mut native| {
                        native.present(self.presentation);
                        self.native = Some(native);
                    })
                    .and_then(|_| self.native.as_ref().unwrap().load(&url))
            };
            self.page.loading = result.is_ok();
            if let Err(error) = result {
                self.page.error = Some(format!("Could not open this page: {error}"));
            }
            window.focus(&self.focus, cx);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = window;
            cx.open_url(&url);
        }
        cx.emit(BrowserEvent::Changed);
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.address.read(cx).text().to_owned();
        self.navigate(&url, window, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            if self.page.error.is_some() {
                if let Some(url) = &self.page.url {
                    let _ = native.load(url);
                }
            } else {
                native.reload();
            }
            self.page.error = None;
        }
        #[cfg(not(target_os = "macos"))]
        self.open_external(cx);
        cx.notify();
    }

    fn open_external(&self, cx: &mut Context<Self>) {
        if let Some(url) = self
            .page
            .url
            .as_deref()
            .filter(|url| model::allowed_navigation(url))
        {
            cx.open_url(url);
        }
    }

    fn history(&mut self, forward: bool) {
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            native.history(forward);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = forward;
    }

    /// Explicitly close even if an async callback temporarily retains an entity.
    fn clear_favicon(&mut self, cx: &mut Context<Self>) {
        if let Some(image) = self.favicon.take() {
            cx.defer(move |cx| gpui::ImageSource::Image(image).evict(None, cx));
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.set_presentation(Presentation::Hidden, cx);
        self.clear_favicon(cx);
        #[cfg(target_os = "macos")]
        {
            if let Some(native) = &mut self.native {
                native.present(Presentation::Hidden);
            }
            self.native = None;
            self.favicon_task = None;
            self.favicon_generation += 1;
        }
    }
}

#[cfg(target_os = "macos")]
impl BrowserSurface {
    fn on_native_event(
        &mut self,
        event: macos::NativeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(native) = &mut self.native else {
            return;
        };
        match event {
            macos::NativeEvent::Changed | macos::NativeEvent::Finished => {
                let finished = matches!(event, macos::NativeEvent::Finished);
                let mut page = native.state();
                // A failed provisional request has no committed WebKit URL.
                if page.url.is_none() {
                    page.url = self.page.url.clone();
                }
                if page.error.is_some() {
                    page.loading = false;
                }
                if page.url != self.page.url || (!self.page.loading && page.loading) {
                    if let Some(image) = self.favicon.take() {
                        cx.defer(move |cx| gpui::ImageSource::Image(image).evict(None, cx));
                    }
                    self.favicon_generation += 1;
                    self.favicon_task = None;
                }
                if !self.address.focus_handle(cx).is_focused(window) {
                    if let Some(url) = &page.url {
                        if self.address.read(cx).text() != url {
                            self.address
                                .update(cx, |input, cx| input.set_text(url.clone(), cx));
                        }
                    }
                    self.address_edited = false;
                }
                native.present(self.presentation);
                if finished && let Some(url) = &page.url {
                    native.discover_favicon(url.clone());
                }
                if page != self.page {
                    self.page = page;
                    cx.emit(BrowserEvent::Changed);
                }
                cx.notify();
            }
            macos::NativeEvent::NewTab(url) => {
                if self.presentation == Presentation::Live {
                    cx.emit(BrowserEvent::NewTab(Some(url)));
                }
            }
            macos::NativeEvent::Key(key) => {
                if self.presentation == Presentation::Live {
                    window.focus(&self.focus, cx);
                    window.defer(cx, move |window, cx| {
                        window.dispatch_keystroke(key, cx);
                    });
                }
            }
            macos::NativeEvent::Snapshot => cx.notify(),
            macos::NativeEvent::Favicon { page, url } => {
                if self.page.url.as_deref() != Some(&page) || !model::allowed_navigation(&url) {
                    return;
                }
                let generation = self.favicon_generation;
                let download = gpui_tokio::Tokio::spawn(cx, async move {
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .redirect(reqwest::redirect::Policy::limited(3))
                        .build()
                        .ok()?;
                    let mut response =
                        client.get(url).send().await.ok()?.error_for_status().ok()?;
                    if response.content_length().is_some_and(|n| n > 1024 * 1024) {
                        return None;
                    }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.ok()? {
                        if bytes.len() + chunk.len() > 1024 * 1024 {
                            return None;
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
                        .with_guessed_format()
                        .ok()?;
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(1024);
                    limits.max_image_height = Some(1024);
                    limits.max_alloc = Some(8 * 1024 * 1024);
                    reader.limits(limits);
                    let icon = reader.decode().ok()?.thumbnail(32, 32);
                    let mut png = std::io::Cursor::new(Vec::new());
                    icon.write_to(&mut png, image::ImageFormat::Png).ok()?;
                    Some(png.into_inner())
                });
                self.favicon_task = Some(cx.spawn(async move |this, cx| {
                    let Ok(Some(bytes)) = download.await else {
                        return;
                    };
                    let _ = this.update(cx, |this, cx| {
                        if this.favicon_generation == generation
                            && this.page.url.as_deref() == Some(&page)
                        {
                            this.favicon = Some(std::sync::Arc::new(gpui::Image::from_bytes(
                                gpui::ImageFormat::Png,
                                bytes,
                            )));
                            cx.emit(BrowserEvent::Changed);
                            cx.notify();
                        }
                    });
                }));
            }
        }
    }
}

#[cfg(feature = "browser-fixture")]
impl BrowserSurface {
    pub fn fixture_history(&mut self, forward: bool) {
        self.history(forward);
    }
    pub fn fixture_native_visible(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.native
                .as_ref()
                .is_some_and(|native| native.fixture_visible())
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    pub fn fixture_snapshot(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.native
                .as_ref()
                .is_some_and(|native| native.snapshot().is_some())
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    pub fn fixture_eval(&self, script: &str) {
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            native.fixture_eval(script);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = script;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn customized_app_shortcuts_win_over_browser_defaults(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut config = crate::settings::KeymapConfig::default();
            bind_keys(cx, &config);
            assert_eq!(
                cx.key_bindings()
                    .borrow()
                    .bindings_for_action(&FocusAddress)
                    .count(),
                1
            );
            assert_eq!(
                cx.key_bindings()
                    .borrow()
                    .bindings_for_action(&Reload)
                    .count(),
                1
            );
            cx.clear_key_bindings();
            config.toggle_sidebar = crate::settings::platform_combo("mod-l");
            config.toggle_changes = "mod-shift-r".into();
            bind_keys(cx, &config);
            assert_eq!(
                cx.key_bindings()
                    .borrow()
                    .bindings_for_action(&FocusAddress)
                    .count(),
                0
            );
            assert_eq!(
                cx.key_bindings()
                    .borrow()
                    .bindings_for_action(&Reload)
                    .count(),
                0
            );
            assert_eq!(
                cx.key_bindings()
                    .borrow()
                    .bindings_for_action(&NewTab)
                    .count(),
                1
            );
        });
    }
}
