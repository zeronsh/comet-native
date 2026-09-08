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

const MAX_MEDIA_BYTES: usize = 64 * 1024 * 1024;
const MAX_MEDIA_ENTRIES: usize = 32;
const MAX_MARKDOWN_BYTES: usize = 2 * 1024 * 1024;
const MAX_PREVIEW_CONTENT_WIDTH: f32 = 900.0;

fn release_media(
    images: impl IntoIterator<Item = super::markdown_media::MediaImage>,
    cx: &mut gpui::App,
) {
    let images: Vec<_> = images.into_iter().map(|media| media.image).collect();
    // After the active window returns to App, release its atlas tiles as well.
    cx.defer(move |cx| {
        for image in images {
            gpui::ImageSource::Image(image).evict(None, cx);
        }
    });
}

pub(super) fn is_markdown(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown"))
}

/// URL path resolution is independent of the UI host's filesystem.
pub(super) fn relative_target(document: &str, target: &str) -> Option<(String, Option<String>)> {
    if let Some(target) = target.strip_prefix("zeron-file:") {
        return relative_target("", target);
    }
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
    media_location: Option<(Option<String>, String)>,
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
    image_generation: u64,
    media_dirty: bool,
    image_allowed: Rc<HashSet<String>>,
    diagram_allowed: Rc<HashSet<String>>,
    image_snapshot: Rc<HashMap<String, Result<super::markdown_media::MediaImage, String>>>,
    diagram_snapshot: Rc<HashMap<String, Result<super::markdown_media::MediaImage, String>>>,
    visible_rows: HashSet<gpui::SharedString>,
    needs_focus: bool,
    suspended: bool,
    selection_pointer: Option<gpui::Point<gpui::Pixels>>,
    selection_task: Option<Task<()>>,
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
        cx.on_release(|view, cx| {
            render::clear_selection_surface(&view.scope);
            let media = view
                .images
                .drain()
                .chain(view.diagrams.drain())
                .filter_map(|(_, result)| result.ok());
            release_media(media, cx);
        })
        .detach();
        Self {
            image_generation: 0,
            media_dirty: true,
            image_allowed: Rc::default(),
            diagram_allowed: Rc::default(),
            image_snapshot: Rc::default(),
            diagram_snapshot: Rc::default(),
            visible_rows: HashSet::new(),
            needs_focus: true,
            suspended: false,
            selection_pointer: None,
            selection_task: None,
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
            media_location: None,
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

    pub fn set_source(&mut self, mut source: String, truncated: bool, cx: &mut Context<Self>) {
        let clipped = source.len() > MAX_MARKDOWN_BYTES;
        if clipped {
            let mut end = MAX_MARKDOWN_BYTES;
            while !source.is_char_boundary(end) {
                end -= 1;
            }
            source.truncate(end);
        }
        render::clear_selection_surface(&self.scope);
        self.epoch = self.epoch.wrapping_add(1);
        self.image_task = None;
        self.diagram_task = None;
        self.preview_image = None;
        self.source_visible.clear();
        let epoch = self.epoch;
        self.loading = self.tree.is_empty();
        self.truncated = truncated || clipped;
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
        self.media_dirty = true;
        self.image_generation = self.image_generation.wrapping_add(1);
        let generation = self.image_generation;
        use futures::{StreamExt as _, stream};
        let sources = super::markdown_media::image_sources(&self.tree);
        self.image_allowed = Rc::new(sources.iter().take(MAX_MEDIA_ENTRIES).cloned().collect());
        let removed: Vec<_> = self
            .images
            .keys()
            .filter(|source| !sources.contains(source))
            .cloned()
            .collect();
        release_media(
            removed
                .into_iter()
                .filter_map(|source| self.images.remove(&source).and_then(Result::ok)),
            cx,
        );
        let Some((client, checkout)) = self.media_client.clone() else {
            for source in sources.into_iter().take(MAX_MEDIA_ENTRIES) {
                self.images
                    .entry(source)
                    .or_insert_with(|| Err("Workspace image connection unavailable".into()));
            }
            return;
        };
        let jobs: Vec<_> = sources
            .into_iter()
            .take(MAX_MEDIA_ENTRIES)
            .filter(|s| !self.images.contains_key(s))
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
                        let read = Box::pin(client.read_image(path, checkout));
                        let deadline = Box::pin(executor.timer(Duration::from_secs(30)));
                        let response = match futures::future::select(read, deadline).await {
                            futures::future::Either::Left((result, _)) => result,
                            futures::future::Either::Right(_) => {
                                Err(super::client::FilesClientError::Transport(
                                    "Image preview timed out".into(),
                                ))
                            }
                        };
                        let result = match response {
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
                        if view.epoch != epoch || view.image_generation != generation {
                            return;
                        }
                        let result = view.admit_media(result);
                        view.images.insert(source, result);
                        view.media_dirty = true;
                        view.list.remeasure_items(0..view.tree.len());
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
        self.media_dirty = true;
        let sources = super::markdown_media::diagram_sources(&self.tree);
        self.diagram_allowed = Rc::new(sources.iter().take(MAX_MEDIA_ENTRIES).cloned().collect());
        let removed: Vec<_> = self
            .diagrams
            .keys()
            .filter(|code| !sources.contains(code))
            .cloned()
            .collect();
        release_media(
            removed
                .into_iter()
                .filter_map(|code| self.diagrams.remove(&code).and_then(Result::ok)),
            cx,
        );
        let style = crate::theme::style_generation();
        if self.diagram_style != style {
            self.preview_image = None;
            release_media(
                self.diagrams.drain().filter_map(|(_, result)| result.ok()),
                cx,
            );
            self.diagram_style = style;
        }
        let palette = crate::markdown::mermaid::Palette::from_theme(Theme::of(cx));
        let jobs: Vec<_> = sources
            .into_iter()
            .take(MAX_MEDIA_ENTRIES)
            .filter(|code| !self.diagrams.contains_key(code))
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
                        let result = view.admit_media(result);
                        view.diagrams.insert(code, result);
                        view.media_dirty = true;
                        view.list.remeasure_items(0..view.tree.len());
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn admit_media(
        &self,
        result: Result<super::markdown_media::MediaImage, String>,
    ) -> Result<super::markdown_media::MediaImage, String> {
        let used: usize = self
            .images
            .values()
            .chain(self.diagrams.values())
            .filter_map(|r| r.as_ref().ok())
            .map(|m| m.bytes)
            .sum();
        result.and_then(|media| {
            if used.saturating_add(media.bytes) > MAX_MEDIA_BYTES {
                Err("Document media preview memory limit reached".into())
            } else {
                Ok(media)
            }
        })
    }

    pub fn activate(
        &mut self,
        path: &str,
        location: &(Option<String>, String),
        cx: &mut Context<Self>,
    ) {
        if self.path != path || self.media_location.as_ref() != Some(location) {
            self.path = path.to_string();
            self.media_location = Some(location.clone());
            self.version = None;
            self.image_generation = self.image_generation.wrapping_add(1);
            self.image_task = None;
            self.preview_image = None;
            release_media(
                self.images.drain().filter_map(|(_, result)| result.ok()),
                cx,
            );
            self.media_dirty = true;
        }
        if self.suspended {
            self.suspended = false;
            self.needs_focus = true;
            self.version = None;
        }
    }

    pub fn suspend(&mut self, cx: &mut Context<Self>) {
        if self.suspended {
            return;
        }
        self.suspended = true;
        self.media_dirty = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.parse_task = None;
        self.image_task = None;
        self.diagram_task = None;
        self.selection_task = None;
        self.preview_image = None;
        self.image_snapshot = Rc::default();
        self.diagram_snapshot = Rc::default();
        self.image_allowed = Rc::default();
        self.diagram_allowed = Rc::default();
        self.version = None;
        self.tree = BlockTree::default();
        self.highlights.clear();
        self.anchors.clear();
        self.cache.borrow_mut().clear();
        render::clear_selection_surface(&self.scope);
        release_media(
            self.images
                .drain()
                .chain(self.diagrams.drain())
                .filter_map(|(_, result)| result.ok()),
            cx,
        );
    }

    pub fn invalidate_images(&mut self, changed: Option<&str>, cx: &mut Context<Self>) {
        let removed: Vec<_> = self
            .images
            .keys()
            .filter(|source| {
                changed.is_none_or(|changed| {
                    relative_target(&self.path, source).is_some_and(|(path, _)| {
                        path == changed || path.starts_with(&format!("{changed}/"))
                    })
                })
            })
            .cloned()
            .collect();
        // Include pending loads in invalidation: a watcher can arrive before the first response.
        let affected = changed.is_none_or(|changed| {
            super::markdown_media::image_sources(&self.tree)
                .iter()
                .any(|source| {
                    relative_target(&self.path, source).is_some_and(|(path, _)| {
                        path == changed || path.starts_with(&format!("{changed}/"))
                    })
                })
        });
        if !affected {
            return;
        }
        self.preview_image = None;
        release_media(
            removed
                .into_iter()
                .filter_map(|source| self.images.remove(&source).and_then(Result::ok)),
            cx,
        );
        if !self.suspended {
            self.load_images(cx);
        }
        self.list.remeasure_items(0..self.tree.len());
        cx.notify();
    }

    fn owns_selection(&self) -> bool {
        crate::markdown::selection::anchor_key().is_some_and(|key| key.starts_with(&self.scope))
    }

    fn selection_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() || !self.owns_selection() || !crate::markdown::selection::is_dragging()
        {
            self.selection_task = None;
            self.selection_pointer = None;
            return;
        }
        self.selection_pointer = Some(event.position);
        if render::update_drag_at(event.position) {
            cx.notify();
        }
        self.schedule_selection_scroll(cx);
    }

    fn schedule_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if self.selection_task.is_some() {
            return;
        }
        let Some(position) = self.selection_pointer else {
            return;
        };
        if crate::transcript::selection_scroll_step(self.list.viewport_bounds(), position) == 0.0 {
            return;
        }
        self.selection_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let _ = this.update(cx, |view, cx| {
                view.selection_task = None;
                if !view.owns_selection() || !crate::markdown::selection::is_dragging() {
                    return;
                }
                let Some(position) = view.selection_pointer else {
                    return;
                };
                render::update_drag_at(position);
                view.list
                    .scroll_by(px(crate::transcript::selection_scroll_step(
                        view.list.viewport_bounds(),
                        position,
                    )));
                cx.notify();
                view.schedule_selection_scroll(cx);
            });
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
            .mx_auto()
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

    #[cfg(test)]
    pub(super) fn test_tree(&self) -> &BlockTree {
        &self.tree
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
                self.visible_rows.insert(opts.row_key.clone());
                opts.cache = Some(self.cache.clone());
                let images = self.image_snapshot.clone();
                let image_owner = cx.weak_entity();
                let image_link = link.clone();
                let diagram_owner = cx.weak_entity();
                let diagrams = self.diagram_snapshot.clone();
                let source_visible = self.source_visible.clone();
                let image_allowed = self.image_allowed.clone();
                let diagram_allowed = self.diagram_allowed.clone();
                opts.media = Some(render::MediaUi {
                    diagram: Some(Rc::new(move |code, id, theme| {
                        let state = diagrams.get(code);
                        let allowed = diagram_allowed.contains(code);
                        let source_shown = source_visible.contains(id.as_ref())
                            || state.is_some_and(Result::is_err)
                            || !allowed;
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
                                        view.list.remeasure_items(0..view.tree.len());
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
                                    div().p(px(12.0)).text_color(theme.text_muted).child(
                                        if diagram_allowed.contains(code) {
                                            "Rendering diagram…"
                                        } else {
                                            "Document diagram preview limit reached"
                                        },
                                    ),
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
                                        .unwrap_or(if image_allowed.contains(&image.source) {
                                            "Loading image…"
                                        } else {
                                            "Document image preview limit reached"
                                        })
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
                        .w_full()
                        .flex()
                        .justify_center()
                        .px(px(24.0))
                        .pb(px(render::MD_BLOCK_GAP))
                        .child(
                            div()
                                .w_full()
                                .max_w(px(MAX_PREVIEW_CONTENT_WIDTH))
                                .min_w_0()
                                .child(render::render_block(
                                    &top.block,
                                    ix,
                                    ix,
                                    &opts,
                                    &theme,
                                    window,
                                    self.highlights.get(&ix).map(|h| h.lines.as_slice()),
                                )),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if self.needs_focus {
            self.needs_focus = false;
            window.focus(&self.focus, cx);
        }
        self.cache
            .borrow_mut()
            .retain_rows(&std::mem::take(&mut self.visible_rows));
        if self.diagram_style != crate::theme::style_generation() {
            self.load_diagrams(cx);
        }
        if self.media_dirty {
            self.media_dirty = false;
            self.image_snapshot = Rc::new(self.images.clone());
            self.diagram_snapshot = Rc::new(self.diagrams.clone());
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
            .on_mouse_move(cx.listener(Self::selection_move))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    if view.owns_selection() {
                        crate::markdown::selection::end_active_drag();
                        view.selection_task = None;
                        view.selection_pointer = None;
                        cx.notify();
                    }
                }),
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

    struct Pair(gpui::Entity<MarkdownPreview>, gpui::Entity<MarkdownPreview>);
    impl Render for Pair {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .child(div().w(px(400.0)).h_full().child(self.0.clone()))
                .child(render::selection_frame_reset())
                .child(div().w(px(400.0)).h_full().child(self.1.clone()))
        }
    }

    #[test]
    fn selection_stays_in_its_document_with_two_markdown_surfaces() {
        gpui_platform::headless().run(|cx| {
            cx.set_global(Theme::dark());
            let make = |text: &str, cx: &mut gpui::App| {
                cx.new(|cx| {
                    let mut view = MarkdownPreview::new("README.md".into(), Rc::new(|_, _| {}), cx);
                    view.tree = parser::parse_full(text);
                    view.list.reset(view.tree.len());
                    view.diagram_style = crate::theme::style_generation();
                    view
                })
            };
            let left = make("Left document text", cx);
            let right = make("Right document text", cx);
            let left_key = format!("{}0:0", left.read(cx).scope);
            let right_key = format!("{}0:0", right.read(cx).scope);
            let window = cx
                .open_window(
                    gpui::WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                            Point::default(),
                            gpui::size(px(800.0), px(600.0)),
                        ))),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|_| Pair(left, right)),
                )
                .unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            })
            .unwrap();
            let left_bounds = render::selection_test_bounds(&left_key);
            let right_bounds = render::selection_test_bounds(&right_key);
            assert!(right_bounds.left() > left_bounds.left());
            crate::markdown::selection::begin(&left_key, 0);
            assert!(render::update_drag_at(gpui::point(
                right_bounds.right(),
                right_bounds.top() + px(3.0)
            )));
            assert_eq!(
                crate::markdown::selection::selected_text().as_deref(),
                Some("Left document text")
            );
            crate::markdown::selection::clear_if_owner(&left_key);
            cx.spawn(async move |cx| {
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }

    #[test]
    fn rendered_image_opens_centered_lightbox_and_escape_restores_focus() {
        gpui_platform::headless().run(|cx| {
            cx.set_global(Theme::dark());
            let window = cx.open_window(gpui::WindowOptions { window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(Point::default(), gpui::size(px(1200.0), px(600.0))))), ..Default::default() }, |_, cx| {
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
            let position = bounds.center();
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

#[cfg(test)]
mod async_tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[gpui::test]
    fn edits_and_suspension_cancel_obsolete_preview_work(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        let view = cx.new(|cx| MarkdownPreview::new("README.md".into(), Rc::new(|_, _| {}), cx));
        view.update(cx, |view, cx| {
            view.set_source("# Old".into(), false, cx);
            view.set_source("# Current".into(), false, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let Block::Heading { runs, .. } = &view.tree.blocks[0].block else {
                panic!("missing heading");
            };
            assert_eq!(runs[0].text, "Current");
        });
        view.update(cx, |view, cx| {
            view.set_source("# Hidden".into(), false, cx);
            view.suspend(cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.tree.is_empty());
            assert!(view.images.is_empty());
            assert!(view.diagrams.is_empty());
        });
        let weak = view.downgrade();
        drop(view);
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
    }

    #[gpui::test]
    fn referenced_image_change_invalidates_cache_and_memory_is_bounded(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        let view =
            cx.new(|cx| MarkdownPreview::new("docs/README.md".into(), Rc::new(|_, _| {}), cx));
        view.update(cx, |view, cx| {
            view.tree = parser::parse_full("![a](../image.png)");
            let mut media = super::super::markdown_media::decode_image(
                "image/svg+xml",
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#.to_vec(),
            )
            .unwrap();
            media.bytes = MAX_MEDIA_BYTES;
            assert!(view.admit_media(Ok(media.clone())).is_ok());
            view.images.insert("../image.png".into(), Ok(media.clone()));
            assert!(view.admit_media(Ok(media)).is_err());
            view.invalidate_images(Some("unrelated.png"), cx);
            assert!(view.images["../image.png"].is_ok());
            view.invalidate_images(Some("image.png"), cx);
            assert!(view.images["../image.png"].is_err());
            let generation = view.image_generation;
            view.activate(
                "docs/README.md",
                &(Some("other-device".into()), "other-checkout".into()),
                cx,
            );
            assert!(view.images.is_empty());
            assert!(view.image_generation > generation);
            assert!(view.version.is_none());
        });
    }
}
