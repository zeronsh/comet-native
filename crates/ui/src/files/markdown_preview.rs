//! Native, virtualized preview of a file's current Markdown buffer.
use crate::{
    markdown::{
        parser::{self, Block, BlockTree},
        render::{self, LinkUi, RenderCache, RenderOptions},
    },
    theme::Theme,
};
use gpui::{
    AnyElement, Context, FocusHandle, ListAlignment, ListOffset, ListSizingBehavior, ListState,
    Render, Task, Window, div, list, prelude::*, px,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

pub(super) fn is_markdown(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown"))
}

/// URL path resolution is independent of the UI host's filesystem.
pub(super) fn relative_target(document: &str, target: &str) -> Option<(String, Option<String>)> {
    if target.starts_with('/') || target.contains(':') || target.contains('\\') {
        return None;
    }
    let (path, anchor) = target
        .split_once('#')
        .map_or((target, None), |(p, a)| (p, Some(a.to_string())));
    let decode = |s: &str| -> Option<String> {
        let mut bytes = Vec::new();
        let mut chars = s.as_bytes().iter().copied();
        while let Some(c) = chars.next() {
            if c == b'%' {
                let a = (chars.next()? as char).to_digit(16)?;
                let b = (chars.next()? as char).to_digit(16)?;
                bytes.push((a * 16 + b) as u8);
            } else {
                bytes.push(c);
            }
        }
        String::from_utf8(bytes).ok()
    };
    let path = decode(path)?;
    if path.contains(['\\', ':', '\0']) || path.starts_with('/') {
        return None;
    }
    if path.is_empty() {
        return Some((document.into(), anchor.and_then(|a| decode(&a))));
    }
    let mut parts: Vec<&str> = document.split('/').collect();
    parts.pop();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some((parts.join("/"), anchor.and_then(|a| decode(&a))))
}

pub(super) struct MarkdownPreview {
    pub focus: FocusHandle,
    pub version: Option<(u64, u64, Option<String>)>,
    path: String,
    scope: String,
    tree: BlockTree,
    list: ListState,
    cache: Rc<RefCell<RenderCache>>,
    highlights: HashMap<usize, Arc<zeron_syntax::HighlightedDocument>>,
    anchors: HashMap<String, usize>,
    parse_task: Option<Task<()>>,
    epoch: u64,
    loading: bool,
    truncated: bool,
    pub media_client: Option<(super::client::WorkspaceFilesClient, String)>,
    images: HashMap<String, Result<super::markdown_media::MediaImage, String>>,
    image_task: Option<Task<()>>,
    diagrams: HashMap<String, Result<super::markdown_media::MediaImage, String>>,
    diagram_task: Option<Task<()>>,
    diagram_style: u32,
    source_visible: HashSet<String>,
    preview_image: Option<crate::attachments::PreviewImage>,
    preview_focus: FocusHandle,
    open_file: Rc<dyn Fn(String, &mut gpui::App)>,
}

impl MarkdownPreview {
    pub fn new(
        path: String,
        open_file: Rc<dyn Fn(String, &mut gpui::App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            diagrams: HashMap::new(),
            diagram_task: None,
            diagram_style: 0,
            source_visible: HashSet::new(),
            preview_image: None,
            preview_focus: cx.focus_handle(),
            media_client: None,
            images: HashMap::new(),
            image_task: None,
            focus: cx.focus_handle(),
            version: None,
            path,
            scope: format!("md-preview-{}|", cx.entity_id()),
            tree: BlockTree::default(),
            list: ListState::new(0, ListAlignment::Top, px(400.0)),
            cache: Rc::new(RefCell::new(RenderCache::default())),
            highlights: HashMap::new(),
            anchors: HashMap::new(),
            parse_task: None,
            epoch: 0,
            loading: false,
            truncated: false,
            open_file,
        }
    }

    pub fn set_source(&mut self, source: String, truncated: bool, cx: &mut Context<Self>) {
        self.epoch += 1;
        let epoch = self.epoch;
        self.loading = self.tree.is_empty();
        self.truncated = truncated;
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let (tree, highlights, anchors) = cx
                .background_executor()
                .spawn(async move {
                    let tree = parser::parse_full(&source);
                    let mut highlights = HashMap::new();
                    let mut anchors = HashMap::new();
                    for (ix, top) in tree.blocks.iter().enumerate() {
                        if let Block::CodeBlock { language, code } = &top.block {
                            if let Ok(doc) =
                                zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                                    source: code,
                                    path: None,
                                    fence_tag: language.as_deref(),
                                })
                            {
                                highlights.insert(ix, Arc::new(doc));
                            }
                        }
                        if let Block::Heading { runs, .. } = &top.block {
                            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
                            let slug: String = text
                                .to_lowercase()
                                .chars()
                                .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                                .map(|c| if c == ' ' { '-' } else { c })
                                .collect();
                            let mut unique = slug.clone();
                            let mut n = 1;
                            while anchors.contains_key(&unique) {
                                unique = format!("{slug}-{n}");
                                n += 1;
                            }
                            anchors.insert(unique, ix);
                        }
                    }
                    (tree, highlights, anchors)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.epoch != epoch {
                    return;
                }
                let offset = view.list.logical_scroll_top();
                view.list.reset(tree.len());
                if !tree.is_empty() {
                    view.list.scroll_to(ListOffset {
                        item_ix: offset.item_ix.min(tree.len() - 1),
                        offset_in_item: offset.offset_in_item,
                    });
                }
                view.tree = tree;
                view.highlights = highlights;
                view.anchors = anchors;
                view.cache.borrow_mut().clear();
                view.loading = false;
                view.load_images(cx);
                view.load_diagrams(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load_images(&mut self, cx: &mut Context<Self>) {
        use futures::{StreamExt as _, stream};
        let sources = super::markdown_media::image_sources(&self.tree);
        self.images.retain(|source, _| sources.contains(source));
        let Some((client, checkout)) = self.media_client.clone() else {
            return;
        };
        let jobs: Vec<_> = sources
            .into_iter()
            .filter(|s| !self.images.contains_key(s))
            .take(32)
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|source| {
                if source.starts_with("https://") || source.starts_with("http://") {
                    return None;
                }
                match relative_target(&self.path, &source) {
                    Some((path, _)) => Some((source, path)),
                    None => {
                        self.images
                            .insert(source, Err("Image path is outside the workspace".into()));
                        None
                    }
                }
            })
            .collect();
        let epoch = self.epoch;
        self.image_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut results = stream::iter(jobs)
                .map(|(source, path)| {
                    let client = client.clone();
                    let checkout = checkout.clone();
                    let executor = executor.clone();
                    async move {
                        let result = match client.read_image(path, checkout).await {
                            Ok((mime, bytes)) => {
                                executor
                                    .spawn(async move {
                                        super::markdown_media::decode_image(&mime, bytes)
                                    })
                                    .await
                            }
                            Err(error) => Err(error.to_string()),
                        };
                        (source, result)
                    }
                })
                .buffer_unordered(3);
            while let Some((source, result)) = results.next().await {
                if this
                    .update(cx, |view, cx| {
                        if view.epoch != epoch {
                            return;
                        }
                        view.images.insert(source, result);
                        view.list.remeasure();
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn load_diagrams(&mut self, cx: &mut Context<Self>) {
        let sources = super::markdown_media::diagram_sources(&self.tree);
        self.diagrams.retain(|code, _| sources.contains(code));
        let style = crate::theme::style_generation();
        if self.diagram_style != style {
            self.diagrams.clear();
            self.diagram_style = style;
        }
        let palette = crate::markdown::mermaid::Palette::from_theme(Theme::of(cx));
        let jobs: Vec<_> = sources
            .into_iter()
            .filter(|code| !self.diagrams.contains_key(code))
            .take(24)
            .collect();
        let epoch = self.epoch;
        self.diagram_task = Some(cx.spawn(async move |this, cx| {
            for code in jobs {
                let source = code.clone();
                let palette = palette.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let svg = crate::markdown::mermaid::render(&source, &palette)?;
                        super::markdown_media::decode_image("image/svg+xml", svg.into_bytes())
                    })
                    .await;
                if this
                    .update(cx, |view, cx| {
                        if view.epoch != epoch || view.diagram_style != style {
                            return;
                        }
                        view.diagrams.insert(code, result);
                        view.list.remeasure();
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn media_element(
        loaded: &super::markdown_media::MediaImage,
        id: gpui::SharedString,
        name: String,
        weak: gpui::WeakEntity<Self>,
    ) -> AnyElement {
        use gpui::StyledImage as _;
        let preview = crate::attachments::PreviewImage {
            name: name.into(),
            image: loaded.image.clone(),
        };
        div()
            .id(id)
            .w_full()
            .max_w(px(loaded.width))
            .max_h(px(480.0))
            .aspect_ratio(loaded.width / loaded.height)
            .cursor_pointer()
            .role(gpui::Role::Button)
            .aria_label("Enlarge image")
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                let _ = weak.update(cx, |view, cx| {
                    view.preview_image = Some(preview.clone());
                    window.focus(&view.preview_focus, cx);
                    cx.notify();
                });
            })
            .child(
                gpui::img(loaded.image.clone())
                    .size_full()
                    .object_fit(gpui::ObjectFit::Contain),
            )
            .into_any_element()
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.render_rows(ix..ix + 1, window, cx)
            .pop()
            .unwrap_or_else(|| gpui::Empty.into_any_element())
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let weak = cx.weak_entity();
        let link = LinkUi {
            handler: Rc::new(move |target, _, cx| {
                weak.update(cx, |view, cx| {
                    let Some((path, anchor)) = relative_target(&view.path, target) else {
                        return !(target.starts_with("https://")
                            || target.starts_with("http://")
                            || target.starts_with("mailto:"));
                    };
                    if path == view.path {
                        if let Some(ix) = anchor.as_ref().and_then(|a| view.anchors.get(a)) {
                            view.list.scroll_to(ListOffset {
                                item_ix: *ix,
                                offset_in_item: px(0.0),
                            });
                            cx.notify();
                        }
                    } else {
                        (view.open_file)(path, cx);
                    }
                    true
                })
                .unwrap_or(true)
            }),
        };
        range
            .filter_map(|ix| {
                let top = self.tree.blocks.get(ix)?;
                let mut opts = RenderOptions::settled(format!("{}{ix}", self.scope).into());
                opts.cache = Some(self.cache.clone());
                let images = self.images.clone();
                let image_owner = cx.weak_entity();
                let image_link = link.clone();
                let diagram_owner = cx.weak_entity();
                let diagrams = self.diagrams.clone();
                let source_visible = self.source_visible.clone();
                opts.media = Some(render::MediaUi {
                    diagram: Some(Rc::new(move |code, id, theme| {
                        let state = diagrams.get(code);
                        let source_shown = source_visible.contains(id.as_ref())
                            || state.is_some_and(Result::is_err);
                        let owner = diagram_owner.clone();
                        let toggle_id = id.to_string();
                        let source = code.to_string();
                        let mut header = super::toolbar(theme)
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child("Mermaid"),
                            )
                            .child(
                                super::toolbar_button(
                                    "mermaid-source",
                                    if source_shown {
                                        "Show diagram"
                                    } else {
                                        "Show source"
                                    },
                                )
                                .on_click(move |_, _, cx| {
                                    let _ = owner.update(cx, |view, cx| {
                                        if !view.source_visible.remove(&toggle_id) {
                                            view.source_visible.insert(toggle_id.clone());
                                        }
                                        view.list.remeasure();
                                        cx.notify();
                                    });
                                })
                                .child(
                                    crate::icons::icon(if source_shown {
                                        crate::icons::EYE
                                    } else {
                                        crate::icons::FILE_CODE
                                    })
                                    .size(px(crate::surface_chrome::ICON_SIZE))
                                    .text_color(theme.text_muted),
                                ),
                            );
                        header = header.child(
                            super::toolbar_button("mermaid-copy", "Copy Mermaid source")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        source.clone(),
                                    ))
                                })
                                .child(
                                    crate::icons::icon(crate::icons::COPY)
                                        .size(px(crate::surface_chrome::ICON_SIZE))
                                        .text_color(theme.text_muted),
                                ),
                        );
                        let mut card = div()
                            .id(id.clone())
                            .w_full()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .rounded(px(crate::surface_chrome::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .child(header);
                        match state {
                            Some(Ok(loaded)) if !source_shown => {
                                card = card.child(Self::media_element(
                                    loaded,
                                    format!("{id}-image").into(),
                                    "Mermaid diagram".into(),
                                    diagram_owner.clone(),
                                ));
                            }
                            Some(Err(error)) => {
                                card = card.child(
                                    div()
                                        .p(px(12.0))
                                        .text_size(px(12.0))
                                        .text_color(theme.warning_muted)
                                        .child(error.clone()),
                                );
                            }
                            None => {
                                card = card.child(
                                    div()
                                        .p(px(12.0))
                                        .text_color(theme.text_muted)
                                        .child("Rendering diagram…"),
                                );
                            }
                            _ => {}
                        }
                        render::DiagramUi {
                            element: card.into_any_element(),
                            show_source: source_shown,
                        }
                    })),
                    image: Rc::new(move |image, id, theme| match images.get(&image.source) {
                        Some(Ok(loaded)) => {
                            let mut el =
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(Self::media_element(
                                        loaded,
                                        id,
                                        if image.alt.is_empty() {
                                            image.source.clone()
                                        } else {
                                            image.alt.clone()
                                        },
                                        image_owner.clone(),
                                    ));
                            if let Some(target) = image.link.clone() {
                                let link = image_link.clone();
                                el = el.child(
                                    super::toolbar_button("markdown-image-link", "Open image link")
                                        .on_click(move |_, window, cx| {
                                            if !(link.handler)(&target, window, cx) {
                                                cx.open_url(&target);
                                            }
                                        })
                                        .child(
                                            crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                                                .size(px(crate::surface_chrome::ICON_SIZE))
                                                .text_color(theme.text_muted),
                                        ),
                                );
                            }
                            el.into_any_element()
                        }
                        state => {
                            let text = if image.source.starts_with("https://")
                                || image.source.starts_with("http://")
                            {
                                format!("{} — {}", image.alt, image.source)
                            } else {
                                format!(
                                    "{} — {}",
                                    image.alt,
                                    state
                                        .and_then(|s| s.as_ref().err())
                                        .map(String::as_str)
                                        .unwrap_or("Loading image…")
                                )
                            };
                            let target = image.source.clone();
                            let external =
                                target.starts_with("https://") || target.starts_with("http://");
                            div()
                                .id(id)
                                .text_color(theme.text_muted)
                                .child(text)
                                .when(external, |el| {
                                    el.cursor_pointer()
                                        .on_click(move |_, _, cx| cx.open_url(&target))
                                })
                                .into_any_element()
                        }
                    }),
                });
                opts.link = Some(link.clone());
                opts.copy = Some(render::CopyUi {
                    handler: Rc::new(|_, code, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_string()))
                    }),
                    copied_ix: None,
                });
                Some(
                    div()
                        .px(px(24.0))
                        .pb(px(render::MD_BLOCK_GAP))
                        .child(render::render_block(
                            &top.block,
                            ix,
                            ix,
                            &opts,
                            &theme,
                            window,
                            self.highlights.get(&ix).map(|h| h.lines.as_slice()),
                        ))
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if self.diagram_style != crate::theme::style_generation() {
            self.load_diagrams(cx);
        }
        let mut root = div()
            .id("markdown-file-preview")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .py(px(16.0))
            .font_family(theme.font_sans.clone())
            .text_color(theme.text)
            .track_focus(&self.focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|view, _, window, cx| window.focus(&view.focus, cx)),
            )
            .on_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "c"
                    && (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
                {
                    if let Some(text) = crate::markdown::selection::selected_text() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        cx.stop_propagation();
                    }
                }
            }))
            .child(render::selection_surface_reset(self.scope.clone()))
            .when(self.loading, |el| {
                el.child(
                    div()
                        .px(px(24.0))
                        .text_color(theme.text_muted)
                        .child("Loading preview…"),
                )
            })
            .when(self.truncated, |el| {
                el.child(
                    div()
                        .px(px(24.0))
                        .text_color(theme.warning_muted)
                        .child("Large file preview is truncated and read-only."),
                )
            })
            .child(
                list(self.list.clone(), cx.processor(Self::render_row))
                    .flex_1()
                    .min_h_0()
                    .with_sizing_behavior(ListSizingBehavior::Auto),
            );
        if let Some(preview) = &self.preview_image {
            let weak = cx.weak_entity();
            root = root.child(crate::attachments::lightbox(
                window.viewport_size(),
                preview,
                &self.preview_focus,
                move |window, cx| {
                    let _ = weak.update(cx, |view, cx| {
                        view.preview_image = None;
                        window.focus(&view.focus, cx);
                        cx.notify();
                    });
                },
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_paths_and_links() {
        assert!(is_markdown("docs/README.MD"));
        assert!(is_markdown("x.markdown"));
        assert!(!is_markdown("x.mdx"));
        assert_eq!(
            relative_target("docs/readme.md", "../a%20b.md#hello"),
            Some(("a b.md".into(), Some("hello".into())))
        );
        assert_eq!(relative_target("readme.md", "../secret"), None);
        assert_eq!(relative_target("docs/readme.md", "%2Fetc/passwd"), None);
        assert_eq!(
            relative_target("docs/readme.md", "https://example.com"),
            None
        );
        assert_eq!(
            relative_target("docs/readme.md", "#hello"),
            Some(("docs/readme.md".into(), Some("hello".into())))
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod layout_tests {
    use super::*;
    use gpui::{AppContext, Bounds, Point};

    #[test]
    fn rendered_image_opens_centered_lightbox_and_escape_restores_focus() {
        gpui_platform::headless().run(|cx| {
            cx.set_global(Theme::dark());
            let window = cx.open_window(gpui::WindowOptions { window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(Point::default(), gpui::size(px(800.0), px(600.0))))), ..Default::default() }, |_, cx| {
                cx.new(|cx| {
                    let mut view = MarkdownPreview::new("README.md".into(), Rc::new(|_, _| {}), cx);
                    view.tree = parser::parse_full("![Example](example.svg)"); view.list.reset(1);
                    view.diagram_style = crate::theme::style_generation();
                    let media = super::super::markdown_media::decode_image("image/svg+xml", br##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="160"><rect width="240" height="160" fill="#468"/></svg>"##.to_vec()).unwrap();
                    view.images.insert("example.svg".into(), Ok(media));
                    view
                })
            }).unwrap();
            let view = window.entity(cx).unwrap();
            cx.update_window(window.into(), |_, window, cx| { window.refresh(); let _ = window.draw(cx); }).unwrap();
            let bounds = view.read(cx).list.bounds_for_item(0).unwrap();
            assert!(bounds.size.height > px(100.0));
            let position = gpui::point(bounds.left() + px(80.0), bounds.top() + px(60.0));
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: gpui::MouseButton::Left, position, click_count: 1, ..Default::default() }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: gpui::MouseButton::Left, position, click_count: 1, ..Default::default() }), cx);
            }).unwrap();
            assert!(view.read(cx).preview_image.is_some());
            cx.update_window(window.into(), |_, window, cx| {
                assert!(view.read(cx).preview_focus.is_focused(window));
                window.refresh(); let _ = window.draw(cx);
                window.dispatch_event(gpui::PlatformInput::KeyDown(gpui::KeyDownEvent { keystroke: gpui::Keystroke::parse("escape").unwrap(), is_held: false, prefer_character_input: false }), cx);
            }).unwrap();
            assert!(view.read(cx).preview_image.is_none());
            cx.update_window(window.into(), |_, window, cx| { assert!(view.read(cx).focus.is_focused(window)); }).unwrap();
            cx.spawn(async move |cx| { cx.update(|cx| cx.quit()); }).detach();
        });
    }
}
