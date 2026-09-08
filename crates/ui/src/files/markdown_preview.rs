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
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Duration};

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
    open_file: Rc<dyn Fn(String, &mut gpui::App)>,
}

impl MarkdownPreview {
    pub fn new(
        path: String,
        open_file: Rc<dyn Fn(String, &mut gpui::App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
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
                cx.notify();
            });
        }));
        cx.notify();
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        div()
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
            )
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
