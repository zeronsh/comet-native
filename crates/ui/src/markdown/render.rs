//! BlockTree → gpui elements.
//!
//! Numbers drive layout (font sizes, line heights, paddings — all constants
//! here); colors are paint. Code blocks render per-line so their height is
//! exactly `lines × line_height`, and syntax highlighting arrives later as
//! recolored `TextRun`s on the identical mono font — layout never changes
//! (mugen's "highlight is pure paint"). Streaming fade-in is an opacity-only
//! animation over the newest block, keyed by stable block identity.

use std::ops::Range;

use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, InteractiveText, SharedString, StyledText, TextRun,
    UnderlineStyle, div, font, prelude::*, px,
};

use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

use super::highlight::{Token, TokenClass};
use super::parser::{Block, BlockTree, InlineRun};

/// Gap between markdown blocks inside one message (comet mdBlockGap).
pub const MD_BLOCK_GAP: f32 = 12.0;
/// Body text size / line height (comet: 14px / 22px).
pub const MD_TEXT_SIZE: f32 = 14.0;
pub const MD_LINE_HEIGHT: f32 = 22.0;
/// Code block metrics — height is `lines × CODE_LINE_HEIGHT + padding + header`.
pub const CODE_TEXT_SIZE: f32 = 12.5;
pub const CODE_LINE_HEIGHT: f32 = 18.0;
pub const CODE_PADDING_X: f32 = 12.0;
pub const CODE_PADDING_Y: f32 = 10.0;

/// Options for one rendered tree (a transcript row or a whole live message).
pub struct RenderOptions {
    /// Stable row key — prefixes element ids (scroll state, animations).
    pub row_key: SharedString,
    /// When set, the last block plays a paint-only fade-in keyed by this value —
    /// pass a stable per-block discriminator (e.g. block index) so each newly
    /// appended block fades exactly once.
    pub fade_last_key: Option<u64>,
}

/// Per-line highlight tokens for a code block, or `None` while pending.
pub type CodeHighlight<'a> = Option<&'a [Vec<Token>]>;

/// Render a whole tree stacked with the md block gap. `highlight` resolves
/// tokens for a top-level block index (code blocks only).
pub fn render_tree(
    tree: &BlockTree,
    opts: &RenderOptions,
    theme: &Theme,
    highlight: &dyn Fn(usize) -> Option<std::sync::Arc<Vec<Vec<Token>>>>,
) -> AnyElement {
    let last = tree.blocks.len().saturating_sub(1);
    div()
        .flex()
        .flex_col()
        .gap(px(MD_BLOCK_GAP))
        .children(tree.blocks.iter().enumerate().map(|(ix, top)| {
            let lines = highlight(ix);
            let el = render_block(
                &top.block,
                ix,
                opts,
                theme,
                lines.as_deref().map(|l| &l[..]),
            );
            if ix == last
                && let Some(key) = opts.fade_last_key
            {
                let id: SharedString = format!("{}-fade{key}", opts.row_key).into();
                fade_in_paint(id, div().child(el))
            } else {
                el
            }
        }))
        .into_any_element()
}

/// Render one block (top-level or nested).
pub fn render_block(
    block: &Block,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    highlight: CodeHighlight,
) -> AnyElement {
    match block {
        Block::Paragraph { runs } => {
            text_element(runs, MD_TEXT_SIZE, MD_LINE_HEIGHT, false, ix, opts, theme)
        }
        Block::Heading { level, runs } => {
            let (size, line) = heading_metrics(*level);
            text_element(runs, size, line, true, ix, opts, theme)
        }
        Block::CodeBlock { language, code } => {
            render_code_block(language.as_deref(), code, ix, opts, theme, highlight)
        }
        Block::BlockQuote { children } => div()
            .border_l_2()
            .border_color(theme.border_strong)
            .pl(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .text_color(theme.text_muted)
            .children(
                children
                    .iter()
                    .enumerate()
                    .map(|(ci, child)| render_block(child, ix * 100 + ci, opts, theme, None)),
            )
            .into_any_element(),
        Block::List {
            ordered_start,
            items,
        } => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(items.iter().enumerate().map(|(item_ix, item)| {
                let marker: SharedString = match ordered_start {
                    Some(start) => format!("{}.", start + item_ix as u64).into(),
                    None => "•".into(),
                };
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(18.0))
                            .text_size(px(MD_TEXT_SIZE))
                            .line_height(px(MD_LINE_HEIGHT))
                            .text_color(theme.text_muted)
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(item.iter().enumerate().map(|(ci, child)| {
                                render_block(child, ix * 100 + item_ix * 10 + ci, opts, theme, None)
                            })),
                    )
            }))
            .into_any_element(),
        Block::Table { header, rows } => {
            let cell = |runs: &[InlineRun], bold: bool, cell_ix: usize| {
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(6.0))
                    .py(px(3.0))
                    .child(text_element(
                        runs,
                        13.0,
                        19.0,
                        bold,
                        ix * 1000 + cell_ix,
                        opts,
                        theme,
                    ))
            };
            div()
                .flex()
                .flex_col()
                .rounded(px(6.0))
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .border_b_1()
                        .border_color(theme.border)
                        .children(header.iter().enumerate().map(|(ci, h)| cell(h, true, ci))),
                )
                .children(rows.iter().enumerate().map(|(ri, row)| {
                    div()
                        .flex()
                        .flex_row()
                        .when(ri + 1 < rows.len(), |el| {
                            el.border_b_1().border_color(theme.border)
                        })
                        .children(
                            row.iter()
                                .enumerate()
                                .map(|(ci, r)| cell(r, false, (ri + 1) * 10 + ci)),
                        )
                }))
                .into_any_element()
        }
        Block::Rule => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.border)
            .into_any_element(),
    }
}

/// Tight monochrome heading scale (comet: h2 ≈ 16px semibold; headings step
/// down quickly toward body size).
fn heading_metrics(level: u8) -> (f32, f32) {
    match level {
        1 => (19.0, 27.0),
        2 => (16.0, 24.0),
        3 => (15.0, 22.0),
        _ => (14.0, 22.0),
    }
}

/// Flattened inline runs: one string + gpui `TextRun`s + clickable link ranges.
pub struct FlatText {
    pub text: String,
    pub runs: Vec<TextRun>,
    pub links: Vec<(Range<usize>, String)>,
}

/// Flatten inline runs into shaped-text inputs. Pure given a theme.
pub fn flatten_runs(runs: &[InlineRun], theme: &Theme, bold_default: bool) -> FlatText {
    let mut text = String::new();
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        let start = text.len();
        text.push_str(&run.text);
        let mut f = if run.style.code {
            font(theme.font_mono.clone())
        } else {
            font(theme.font_sans.clone())
        };
        f.weight = if run.style.bold || bold_default {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        f.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        // Links stay monochrome — foreground with an underline (comet's md
        // theme underlines in the text color; indigo is reserved for primary
        // actions).
        let is_link = run.style.link.is_some();
        let color = theme.text;
        if let Some(url) = &run.style.link {
            // Merge adjacent runs of the same link into one clickable range.
            match links.last_mut() {
                Some((range, last_url)) if range.end == start && last_url == url => {
                    range.end = text.len();
                }
                _ => links.push((start..text.len(), url.clone())),
            }
        }
        out.push(TextRun {
            len: run.text.len(),
            font: f,
            color,
            // Inline code: mono over a faint white wash (comet: white at 8%).
            background_color: run.style.code.then_some(crate::theme::white_alpha(0.08)),
            underline: is_link.then_some(UnderlineStyle {
                color: Some(theme.text_muted),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_muted),
            }),
        });
    }
    FlatText {
        text,
        runs: out,
        links,
    }
}

fn text_element(
    runs: &[InlineRun],
    size: f32,
    line_height: f32,
    bold_default: bool,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    let flat = flatten_runs(runs, theme, bold_default);
    let styled = StyledText::new(flat.text).with_runs(flat.runs);
    let inner: AnyElement = if flat.links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = flat.links.into_iter().unzip();
        let id: SharedString = format!("{}-t{ix}", opts.row_key).into();
        InteractiveText::new(id, styled)
            .on_click(ranges, move |clicked_ix, _window, cx| {
                if let Some(url) = urls.get(clicked_ix) {
                    cx.open_url(url);
                }
            })
            .into_any_element()
    };
    div()
        .text_size(px(size))
        .line_height(px(line_height))
        .child(inner)
        .into_any_element()
}

fn render_code_block(
    language: Option<&str>,
    code: &str,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    highlight: CodeHighlight,
) -> AnyElement {
    let mono = font(theme.font_mono.clone());
    let lines: Vec<&str> = code.split('\n').collect();
    let scroll_id: SharedString = format!("{}-code{ix}", opts.row_key).into();
    div()
        .rounded(px(10.0))
        // Faint white wash over the near-black panel ≈ #101010 (comet's code
        // surface), with the hairline border.
        .bg(crate::theme::white_alpha(0.035))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .when_some(language, |el, lang| {
            el.child(
                div()
                    .px(px(CODE_PADDING_X))
                    .py(px(5.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(lang.to_string())),
            )
        })
        .child(
            div()
                .id(scroll_id)
                .overflow_x_scroll()
                .px(px(CODE_PADDING_X))
                .py(px(CODE_PADDING_Y))
                .font_family(theme.font_mono.clone())
                .text_size(px(CODE_TEXT_SIZE))
                .line_height(px(CODE_LINE_HEIGHT))
                .whitespace_nowrap()
                .flex()
                .flex_col()
                .children(lines.iter().enumerate().map(|(li, line)| {
                    let tokens = highlight
                        .and_then(|h| h.get(li))
                        .map(|t| &t[..])
                        .unwrap_or(&[]);
                    div().h(px(CODE_LINE_HEIGHT)).flex_none().child(
                        StyledText::new(line.to_string())
                            .with_runs(runs_for_code_line(line, tokens, &mono, theme)),
                    )
                })),
        )
        .into_any_element()
}

/// Paint color for a token class — a monochrome hierarchy (comet's code blocks
/// carry no hue): keywords full-bright, strings a step down, comments faint.
pub fn token_color(class: TokenClass, theme: &Theme) -> Hsla {
    match class {
        TokenClass::Keyword => theme.text,
        TokenClass::StringLit => theme.text_muted,
        TokenClass::Comment => theme.text_faint,
        TokenClass::Number => theme.text_muted,
    }
}

/// Build the exact-cover `TextRun` list for one code line from its tokens.
/// Same font everywhere — recoloring can never change layout.
pub fn runs_for_code_line(
    line: &str,
    tokens: &[Token],
    mono: &gpui::Font,
    theme: &Theme,
) -> Vec<TextRun> {
    let plain = |len: usize| TextRun {
        len,
        font: mono.clone(),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::new();
    let mut at = 0usize;
    for token in tokens {
        if token.range.start > at {
            runs.push(plain(token.range.start - at));
        }
        let mut run = plain(token.range.len());
        run.color = token_color(token.class, theme);
        runs.push(run);
        at = token.range.end;
    }
    if at < line.len() {
        runs.push(plain(line.len() - at));
    }
    runs.retain(|r| r.len > 0);
    runs
}

/// Paint-only fade-in (opacity 0→1 over the entrance curve; no translation, so
/// the veil can never affect layout).
pub fn fade_in_paint<E>(id: impl Into<gpui::ElementId>, element: E) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    element
        .with_animation(id, motion::FADE_IN.animation(), |el, t| el.opacity(t))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::highlight::{Lang, tokenize_line};
    use crate::markdown::parser::InlineStyle;

    #[test]
    fn code_line_runs_cover_exactly() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let line = r#"let x = "hi"; // done"#;
        let (tokens, _) = tokenize_line(Lang::Rust, line, Default::default());
        let runs = runs_for_code_line(line, &tokens, &mono, &theme);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, line.len());
        assert!(
            runs.iter().all(|r| r.font == mono),
            "highlight must not change fonts"
        );
        // At least one non-plain color made it through.
        assert!(runs.iter().any(|r| r.color != theme.text));
    }

    #[test]
    fn code_line_runs_with_no_tokens_are_one_plain_run() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let runs = runs_for_code_line("plain text", &[], &mono, &theme);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 10);
    }

    #[test]
    fn flatten_runs_maps_links_and_styles() {
        let theme = Theme::dark();
        let runs = vec![
            InlineRun {
                text: "go ".into(),
                style: InlineStyle::default(),
            },
            InlineRun {
                text: "here".into(),
                style: InlineStyle {
                    link: Some("https://x.dev".into()),
                    ..Default::default()
                },
            },
            InlineRun {
                text: " now".into(),
                style: InlineStyle {
                    bold: true,
                    ..Default::default()
                },
            },
        ];
        let flat = flatten_runs(&runs, &theme, false);
        assert_eq!(flat.text, "go here now");
        assert_eq!(flat.links, vec![(3..7, "https://x.dev".to_string())]);
        let total: usize = flat.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, flat.text.len());
        // Links stay monochrome (foreground + underline), never accent-tinted.
        assert_eq!(flat.runs[1].color, theme.text);
        assert!(flat.runs[1].underline.is_some());
        assert_eq!(flat.runs[2].font.weight, FontWeight::SEMIBOLD);
    }

    #[test]
    fn adjacent_same_link_runs_merge_into_one_range() {
        let theme = Theme::dark();
        let style = InlineStyle {
            link: Some("https://x.dev".into()),
            ..Default::default()
        };
        let runs = vec![
            InlineRun {
                text: "bold".into(),
                style: InlineStyle {
                    bold: true,
                    ..style.clone()
                },
            },
            InlineRun {
                text: " tail".into(),
                style,
            },
        ];
        let flat = flatten_runs(&runs, &theme, false);
        assert_eq!(flat.links, vec![(0..9, "https://x.dev".to_string())]);
    }
}
