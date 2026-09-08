use super::{BrowserEvent, BrowserSurface};
use crate::{icons, surface_chrome, theme::Theme};
use gpui::{
    AnyElement, Context, Focusable, IntoElement, KeyDownEvent, MouseButton, Render, Window, div,
    prelude::*, px,
};

fn button(
    id: &'static str,
    label: &'static str,
    glyph: &'static str,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    // Match Files/History chrome, including focus-preserving mouse-down and
    // tooltips. Disabled controls have neither a pointer cursor nor a handler.
    crate::files::toolbar_button(id, label)
        .when(!enabled, |el| el.cursor_default().opacity(0.35))
        .child(
            icons::icon(glyph)
                .size(px(surface_chrome::ICON_SIZE))
                .text_color(theme.text_muted),
        )
}

impl BrowserSurface {
    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let address_focused = self.address.focus_handle(cx).is_focused(window);
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        if primary && !mods.alt && !(cfg!(target_os = "macos") && mods.control) {
            match (key, mods.shift) {
                ("l", false) => self.focus_address(window, cx),
                ("t", false) => cx.emit(BrowserEvent::NewTab(None)),
                ("w", false) => cx.emit(BrowserEvent::Close),
                ("r", true) => self.reload(cx),
                ("[", false) => self.history(false),
                ("]", false) => self.history(true),
                _ => return,
            }
        } else if address_focused && !primary && !mods.alt {
            match key {
                "enter" if !mods.shift => self.submit(window, cx),
                "escape" => {
                    let url = self.page.url.clone().unwrap_or_default();
                    self.address.update(cx, |input, cx| input.set_text(url, cx));
                    self.address_edited = false;
                    self.validation = None;
                    window.focus(&self.focus, cx);
                }
                _ => return,
            }
        } else {
            return;
        }
        cx.stop_propagation();
    }

    fn empty_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let external = !cfg!(target_os = "macos");
        let has_error = self.page.error.is_some();
        let title = if has_error {
            "Couldn’t load this page"
        } else if external && self.page.url.is_some() {
            "Opened in your browser"
        } else {
            "A little room to explore"
        };
        let description = if let Some(error) = &self.page.error {
            error.clone()
        } else if external {
            "Open a website or local app in your default browser. Embedded browsing is available on macOS.".into()
        } else {
            "Preview your local app or keep a website beside your conversation.".into()
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(300.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .size(px(44.0))
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_raised.opacity(0.5))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                icons::icon(icons::GLOBE)
                                    .size(px(22.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_center()
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted)
                            .child(description),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .id("browser-empty-action")
                            .h(px(28.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_raised)
                            .cursor_pointer()
                            .role(gpui::Role::Button)
                            .aria_label(if has_error {
                                "Retry page"
                            } else {
                                "Enter an address"
                            })
                            .hover(|style| style.bg(crate::theme::wash(0.10)))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if has_error {
                                    this.reload(cx);
                                } else {
                                    this.focus_address(window, cx);
                                }
                            }))
                            .child(if has_error {
                                "Try again"
                            } else {
                                "Enter an address"
                            })
                            .when(!has_error, |el| {
                                el.child(
                                    div()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_faint)
                                        .child(if cfg!(target_os = "macos") {
                                            "⌘L"
                                        } else {
                                            "Ctrl L"
                                        }),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let focused = self.address.focus_handle(cx).is_focused(window);
        let external = !cfg!(target_os = "macos");
        let has_page = self.page.url.is_some();
        let back = button(
            "browser-back",
            "Back",
            icons::ARROW_LEFT,
            self.page.can_back,
            &theme,
        )
        .when(self.page.can_back, |el| {
            el.on_click(cx.listener(|this, _, _, _| this.history(false)))
        });
        let forward = button(
            "browser-forward",
            "Forward",
            icons::ARROW_RIGHT,
            self.page.can_forward,
            &theme,
        )
        .when(self.page.can_forward, |el| {
            el.on_click(cx.listener(|this, _, _, _| this.history(true)))
        });
        let reload = button(
            "browser-reload",
            "Reload page",
            icons::REFRESH,
            has_page,
            &theme,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
        });
        let address = div()
            .id("browser-address")
            .flex_1()
            .min_w_0()
            .h(px(26.0))
            .px(px(7.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(if self.validation.is_some() {
                theme.danger
            } else if focused {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.surface_raised.opacity(0.65))
            .flex()
            .items_center()
            .gap(px(6.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    #[cfg(target_os = "macos")]
                    if let Some(native) = &this.native {
                        native.focus_chrome();
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = this;
                }),
            )
            .child(
                icons::icon(icons::GLOBE)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_faint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(18.0))
                    .overflow_hidden()
                    .child(self.address.clone()),
            )
            .when(focused || self.address_edited, |el| {
                el.child(
                    div()
                        .id("browser-go")
                        .size(px(18.0))
                        .flex_none()
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label("Go to address")
                        .hover(|s| s.bg(crate::theme::wash(0.10)))
                        .on_mouse_down(MouseButton::Left, |_, w, _| w.prevent_default())
                        .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx)))
                        .child(
                            icons::icon(icons::RETURN)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
            });
        let open = button(
            "browser-external",
            "Open in default browser",
            icons::ARROW_UP_RIGHT,
            has_page,
            &theme,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.open_external(cx)))
        });
        let toolbar = surface_chrome::toolbar(&theme)
            .when(!external, |el| el.child(back).child(forward).child(reload))
            .child(address)
            .child(open);

        let body = div()
            .id("browser-page")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.bg);
        #[cfg(target_os = "macos")]
        let body = if let Some(native) = &self.native {
            if self.page.error.is_some() {
                body.child(self.empty_body(&theme, cx))
            } else {
                let snapshot = native.snapshot();
                let native = native.handle();
                body.when_some(snapshot, |body, image| {
                    body.child(
                        gpui::img(image)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Fill),
                    )
                })
                .child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            native.borrow_mut().sync(bounds, window.scale_factor());
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
            }
        } else {
            body.child(self.empty_body(&theme, cx))
        };
        #[cfg(not(target_os = "macos"))]
        let body = body.child(self.empty_body(&theme, cx));

        let remote_loopback = self.remote
            && self
                .page
                .url
                .as_deref()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|u| super::model::loopback(&u));
        div().id("browser-surface").size_full().flex().flex_col().track_focus(&self.focus)
            .key_context("Browser").on_key_down(cx.listener(Self::key_down))
            .child(toolbar)
            .when_some(self.validation.clone(), |el, message| el.child(div().px(px(12.0)).py(px(8.0)).text_size(crate::typography::ui_rems(11.0)).text_color(theme.danger).child(message)))
            .when(remote_loopback, |el| el.child(div().px(px(12.0)).py(px(8.0)).border_b_1().border_color(theme.border)
                .text_size(crate::typography::ui_rems(11.0)).text_color(theme.text_muted)
                .child("Localhost opens on this device. For your remote session, use a reachable server address.")))
            .child(body)
            .when(external, |el| el.child(div().h(px(26.0)).px(px(10.0)).flex().items_center().gap(px(5.0)).border_t_1().border_color(theme.border)
                .text_size(crate::typography::ui_rems(10.0)).text_color(theme.text_faint)
                .child(icons::icon(icons::ARROW_UP_RIGHT).size(px(11.0))).child("Opens in your default browser")))
    }
}
