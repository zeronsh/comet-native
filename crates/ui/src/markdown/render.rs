//! BlockTree → gpui elements.
//!
//! Numbers drive layout (font sizes, line heights, paddings — all constants
//! here); colors are paint. Code blocks render per-line so their height is
//! exactly `lines × line_height`, and syntax highlighting arrives later as
//! recolored `TextRun`s on the identical mono font — layout never changes
//! (mugen's "highlight is pure paint"). Streaming fade-in is a per-appended-
//! chunk opacity veil over the text runs (see [`super::veil`]) — opacity only,
//! zero translate, applied after layout-relevant properties are fixed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, BorderStyle, Bounds, FontStyle, FontWeight, Hsla, InteractiveText, SharedString,
    StyledText, TextRun, UnderlineStyle, Window, canvas, div, font, point, prelude::*, px, quad,
    size,
};

use crate::theme::Theme;

use super::highlight::{Token, TokenClass};
use super::parser::{Block, BlockTree, InlineRun, TableAlign};
use super::veil::{RowVeil, apply_veil, slice_spans};

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

// Table metrics — a port of mugen-markdown 0.6.2's `TableBlock` under comet's
// resolved md theme. The design is frameless ("flat hairline"): 1px horizontal
// rules under the header and between rows are the only chrome — no outer box,
// no header fill, no corner radius (theme: headerBackground transparent,
// radius 0). Cells use the body scale (14/22) with a uniform 12px padding;
// the header row is weight-700 per `table.headerWeight`.
/// Uniform cell padding in px (comet `table.cellPadding`).
pub const TABLE_CELL_PADDING: f32 = 12.0;
/// Hairline between rows in px (comet `table.gap`).
pub const TABLE_DIVIDER: f32 = 1.0;
/// Header row font weight (comet `table.headerWeight` = 700).
pub const TABLE_HEADER_WEIGHT: FontWeight = FontWeight::BOLD;
/// Floor for a column's max-content share, so a short column ("1k") beside a
/// prose column keeps a readable width (mugen `MIN_COLUMN_CONTENT`).
pub const TABLE_MIN_COLUMN_CONTENT: f32 = 48.0;
/// Minimum rendered column width in px, padding included (comet
/// `table.minColumnWidth`). Naturally narrower columns keep their content
/// width; wider ones wrap down to this floor, then the table scrolls.
pub const TABLE_MIN_COLUMN_WIDTH: f32 = 96.0;
/// Hairline tone (comet md theme `table.borderColor`: rgba(255,255,255,0.1)).
pub fn table_hairline() -> Hsla {
    crate::theme::white_alpha(0.10)
}

/// Options for one rendered tree (a transcript row or a whole live message).
pub struct RenderOptions {
    /// Stable row key — prefixes element ids (scroll state, animations).
    pub row_key: SharedString,
    /// Streaming veil state for a live row: newly appended text fades in via
    /// paint-only run opacity, keyed per (element, chunk offset) so each chunk
    /// fades exactly once. `None` renders without fades (completed rows).
    pub veil: Option<Rc<RefCell<RowVeil>>>,
    /// Flatten/shape input cache (see [`RenderCache`]): settled blocks reuse
    /// their flat text + runs across frames instead of rebuilding them — the
    /// per-frame cost of a fading live row stays O(tail block), flat in the
    /// total reply length. `None` rebuilds every pass.
    pub cache: Option<Rc<RefCell<RenderCache>>>,
    /// Frame timestamp driving veil opacities (one clock per render pass).
    pub now: Instant,
    /// Code-block copy-button plumbing (round 9): `None` renders no button
    /// (previews outside the transcript).
    pub copy: Option<CopyUi>,
}

/// Copy-button wiring for one row's code blocks: the handler writes the code
/// to the clipboard and flips a transient per-row "Copied" state owned by the
/// transcript entity; `copied_ix` is the block currently showing feedback.
#[derive(Clone)]
pub struct CopyUi {
    pub handler: Rc<dyn Fn(usize, SharedString, &mut Window, &mut gpui::App)>,
    pub copied_ix: Option<usize>,
}

impl RenderOptions {
    /// Options for a completed (non-streaming) row — no veil, no cache.
    pub fn settled(row_key: SharedString) -> Self {
        Self {
            row_key,
            veil: None,
            cache: None,
            now: Instant::now(),
            copy: None,
        }
    }
}

/// Cross-frame cache of flatten results, keyed by
/// `(row key, top-level block ix, element discriminator)`.
///
/// During a streaming fade the live row re-renders every frame; without the
/// cache each frame re-derives every block's flat `String` + `TextRun`s —
/// O(reply length) per frame, growing through long replies. The incremental
/// parser only ever touches a suffix of the top-level blocks
/// ([`super::parser::IncrementalParser::stable_prefix_blocks`]), so everything
/// below that boundary is byte-identical and its flatten result (and, via
/// gpui's line-layout cache keyed on identical text+runs, its shaping) can be
/// reused as-is. `SharedString`/`Rc` make the reuse O(1) per block.
#[derive(Default)]
pub struct RenderCache {
    flats: HashMap<(SharedString, usize, usize), Rc<FlatText>>,
    code: HashMap<(SharedString, usize, usize), Rc<CachedCode>>,
}

/// Cached per-line code runs (validity: code length + highlight identity).
pub struct CachedCode {
    code_len: usize,
    /// Slice-pointer identity + len of the highlight Arc that produced this.
    hl_key: (usize, usize),
    lines: Vec<(SharedString, Vec<TextRun>)>,
}

impl RenderCache {
    /// Drop every cached entry for `row`.
    pub fn invalidate_row(&mut self, row: &str) {
        self.flats.retain(|(r, _, _), _| r.as_ref() != row);
        self.code.retain(|(r, _, _), _| r.as_ref() != row);
    }

    pub fn clear(&mut self) {
        self.flats.clear();
        self.code.clear();
    }
}

/// Per-line highlight tokens for a code block, or `None` while pending.
pub type CodeHighlight<'a> = Option<&'a [Vec<Token>]>;

/// Render a whole tree stacked with the md block gap. `highlight` resolves
/// tokens for a top-level block index (code blocks only).
pub fn render_tree(
    tree: &BlockTree,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: &dyn Fn(usize) -> Option<std::sync::Arc<Vec<Vec<Token>>>>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(MD_BLOCK_GAP))
        .children(tree.blocks.iter().enumerate().map(|(ix, top)| {
            let lines = highlight(ix);
            render_block(
                &top.block,
                ix,
                ix,
                opts,
                theme,
                window,
                lines.as_deref().map(|l| &l[..]),
            )
        }))
        .into_any_element()
}

/// Render one block (top-level or nested). `top_ix` is the enclosing top-level
/// block index (cache invalidation scope); `ix` the per-element discriminator.
#[allow(clippy::too_many_arguments)]
pub fn render_block(
    block: &Block,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: CodeHighlight,
) -> AnyElement {
    match block {
        Block::Paragraph { runs } => text_element(
            runs,
            MD_TEXT_SIZE,
            MD_LINE_HEIGHT,
            false,
            top_ix,
            ix,
            opts,
            theme,
        ),
        Block::Heading { level, runs } => {
            let (size, line) = heading_metrics(*level);
            text_element(runs, size, line, true, top_ix, ix, opts, theme)
        }
        Block::CodeBlock { language, code } => {
            render_code_block(language.as_deref(), code, top_ix, ix, opts, theme, highlight)
        }
        Block::BlockQuote { children } => div()
            .border_l_2()
            .border_color(theme.border_strong)
            .pl(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .text_color(theme.text_muted)
            .children(children.iter().enumerate().map(|(ci, child)| {
                render_block(child, top_ix, ix * 100 + ci, opts, theme, window, None)
            }))
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
                                render_block(
                                    child,
                                    top_ix,
                                    ix * 100 + item_ix * 10 + ci,
                                    opts,
                                    theme,
                                    window,
                                    None,
                                )
                            })),
                    )
            }))
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => render_table(header, rows, align, top_ix, ix, opts, theme, window),
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

/// Shared per-column table geometry (port of mugen `tableColumns`).
pub struct TableColumns {
    /// Per-column max-content width, padding included.
    pub naturals: Vec<f32>,
    /// Per-column minimum width, padding included = `min(natural, minColumnWidth)`.
    pub minimums: Vec<f32>,
    /// Σ minimums — the width below which the table stops shrinking and scrolls.
    pub min_table_width: f32,
}

/// Resolve column geometry from measured per-column max-content widths
/// (content only — padding is added here, as the source adds `2 * cellPadding`).
pub fn table_columns(content_widths: &[f32]) -> TableColumns {
    let naturals: Vec<f32> = content_widths
        .iter()
        .map(|w| w.max(TABLE_MIN_COLUMN_CONTENT) + 2.0 * TABLE_CELL_PADDING)
        .collect();
    let minimums: Vec<f32> = naturals
        .iter()
        .map(|n| n.min(TABLE_MIN_COLUMN_WIDTH))
        .collect();
    let min_table_width = minimums.iter().sum();
    TableColumns {
        naturals,
        minimums,
        min_table_width,
    }
}

/// Element/cache discriminator for a table cell (row-major under the block ix).
fn table_cell_ix(ix: usize, r: usize, c: usize) -> usize {
    ix * 100_000 + r * 100 + c
}

/// A GFM table — a port of mugen-markdown's `TableBlock` under comet's md
/// theme (see the `TABLE_*` constants).
///
/// Column widths resolve exactly the way the source's CSS does: each cell is
/// `flex: <max-content> <max-content> 0; min-width: min(max-content, 96px)`,
/// so widths are content-proportional with a readable per-column floor.
/// Naturals come from shaping each cell's runs unwrapped (gpui's line-layout
/// cache makes repeat frames cheap); the flex resolution itself is Taffy's —
/// the same algorithm as the web's. When even the floors no longer fit, the
/// rows overflow the viewport and the table scrolls horizontally instead of
/// crushing every column into per-character wrapping.
#[allow(clippy::too_many_arguments)]
fn render_table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
) -> AnyElement {
    // Header row first, mirroring the source's `rows` shape (rows may be ragged).
    let all: Vec<&[Vec<InlineRun>]> = std::iter::once(header)
        .filter(|h| !h.is_empty())
        .map(|h| h as &[Vec<InlineRun>])
        .chain(rows.iter().map(|r| r.as_slice()))
        .collect();
    let cols = all.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return gpui::Empty.into_any_element();
    }
    let has_header = !header.is_empty();

    // Flatten every cell (cache-aware) and take per-column max-content widths.
    let text_system = window.text_system();
    let mut flats: Vec<Vec<Option<Rc<FlatText>>>> = Vec::with_capacity(all.len());
    let mut content = vec![0.0f32; cols];
    for (r, row) in all.iter().enumerate() {
        let weight = if has_header && r == 0 {
            TABLE_HEADER_WEIGHT
        } else {
            FontWeight::NORMAL
        };
        let mut out: Vec<Option<Rc<FlatText>>> = Vec::with_capacity(cols);
        for (c, natural) in content.iter_mut().enumerate() {
            let Some(runs) = row.get(c) else {
                out.push(None);
                continue;
            };
            let flat = flatten_cached(runs, weight, top_ix, table_cell_ix(ix, r, c), opts, theme);
            if !flat.text.is_empty() {
                // Cell sources are single-line; guard anyway (same byte count,
                // so the runs still cover the text exactly).
                let line: SharedString = if flat.text.contains('\n') {
                    flat.text.replace('\n', " ").into()
                } else {
                    flat.text.clone()
                };
                let width = f32::from(
                    text_system
                        .shape_line(line, px(MD_TEXT_SIZE), &flat.runs, None)
                        .width(),
                );
                if width > *natural {
                    *natural = width;
                }
            }
            out.push(Some(flat));
        }
        flats.push(out);
    }
    let geo = table_columns(&content);

    // Frameless flat-hairline chrome: 1px rules under the header and between
    // rows are the only paint (`table.gap` = 1, borderColor white@10%); the
    // theme's headerBackground is transparent and its radius 0, so there is no
    // header fill, outer box, or rounding.
    let hairline = table_hairline();
    let mut inner = div()
        .flex()
        .flex_col()
        .w_full()
        .min_w(px(geo.min_table_width));
    for (r, row) in flats.iter().enumerate() {
        if r > 0 {
            inner = inner.child(div().flex_none().h(px(TABLE_DIVIDER)).w_full().bg(hairline));
        }
        let mut row_el = div().flex().flex_row();
        for (c, cell_flat) in row.iter().enumerate() {
            let mut cell = div()
                .flex_grow(geo.naturals[c])
                .flex_shrink(geo.naturals[c])
                .flex_basis(px(0.0))
                .min_w(px(geo.minimums[c]))
                .p(px(TABLE_CELL_PADDING))
                .text_size(px(MD_TEXT_SIZE))
                .line_height(px(MD_LINE_HEIGHT));
            cell = match align.get(c).copied().unwrap_or_default() {
                TableAlign::Left => cell,
                TableAlign::Center => cell.text_center(),
                TableAlign::Right => cell.text_right(),
            };
            if let Some(flat) = cell_flat {
                cell = cell.child(flat_text_element(flat, table_cell_ix(ix, r, c), opts));
            }
            row_el = row_el.child(cell);
        }
        inner = inner.child(row_el);
    }

    // The horizontal scroller — when the floors exceed the viewport the inner
    // block keeps `min_table_width` and this viewport scrolls it.
    let scroll_id: SharedString = format!("{}-table{ix}", opts.row_key).into();
    div()
        .id(scroll_id)
        .w_full()
        .overflow_x_scroll()
        .child(inner)
        .into_any_element()
}

/// Flattened inline runs: one string + gpui `TextRun`s + clickable link ranges
/// + inline-code ranges (their rounded washes are painted by a canvas UNDER
/// the text — `TextRun::background_color` can only paint square boxes).
/// `text` is a `SharedString` so cached reuse across frames is an Arc clone.
pub struct FlatText {
    pub text: SharedString,
    pub runs: Vec<TextRun>,
    pub links: Vec<(Range<usize>, String)>,
    pub code_ranges: Vec<Range<usize>>,
}

/// Inline-code tint (round 9): the original is neutral (chat-view.tsx mdTheme
/// `inlineCode: #f0f0f0 on white/8%`), but the user asked for "a nice purple"
/// — violet-300 text over a violet-400 wash, readable on the #060606 panel.
pub fn inline_code_text() -> Hsla {
    crate::theme::oklch(0.811, 0.111, 293.571) // violet-300
}
pub fn inline_code_wash() -> Hsla {
    crate::theme::oklch(0.702, 0.183, 293.541).opacity(0.12) // violet-400/12
}
/// Rounded-wash geometry: small radius on a slightly inset box (paint-only —
/// x extends 2px past the glyphs, y insets 2px from the 22px line box).
pub const INLINE_CODE_RADIUS: f32 = 4.5;
pub const INLINE_CODE_PAD_X: f32 = 2.0;
pub const INLINE_CODE_INSET_Y: f32 = 2.0;

/// Flatten inline runs into shaped-text inputs. Pure given a theme.
pub fn flatten_runs(runs: &[InlineRun], theme: &Theme, bold_default: bool) -> FlatText {
    flatten_runs_weighted(
        runs,
        theme,
        if bold_default {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        },
    )
}

/// [`flatten_runs`] with an explicit base weight (table headers are 700 per
/// comet's `table.headerWeight`; strong runs never drop below semibold).
fn flatten_runs_weighted(runs: &[InlineRun], theme: &Theme, base_weight: FontWeight) -> FlatText {
    let mut text = String::new();
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    let mut code_ranges: Vec<Range<usize>> = Vec::new();
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
        f.weight = if run.style.bold && base_weight.0 < FontWeight::SEMIBOLD.0 {
            FontWeight::SEMIBOLD
        } else {
            base_weight
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
        // Inline code reads violet (see `inline_code_text`); everything else
        // stays the monochrome foreground.
        let color = if run.style.code {
            inline_code_text()
        } else {
            theme.text
        };
        if run.style.code {
            // Merge adjacent code runs into one wash box (like links below).
            match code_ranges.last_mut() {
                Some(range) if range.end == start => range.end = text.len(),
                _ => code_ranges.push(start..text.len()),
            }
        }
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
            // Inline code's wash is painted as ROUNDED quads by the canvas
            // underlay (`code_wash_underlay`) — a run background here could
            // only be a square box.
            background_color: None,
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
        text: text.into(),
        runs: out,
        links,
        code_ranges,
    }
}

/// Flatten through the cross-frame cache when one is wired: settled blocks
/// reuse text + runs untouched (O(1) per block per frame); only blocks the
/// incremental parser invalidated rebuild.
fn flatten_cached(
    runs: &[InlineRun],
    base_weight: FontWeight,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> Rc<FlatText> {
    match &opts.cache {
        Some(cache) => cache
            .borrow_mut()
            .flats
            .entry((opts.row_key.clone(), top_ix, ix))
            .or_insert_with(|| Rc::new(flatten_runs_weighted(runs, theme, base_weight)))
            .clone(),
        None => Rc::new(flatten_runs_weighted(runs, theme, base_weight)),
    }
}

/// Veiled, clickable text for a flattened block (no sizing wrapper).
fn flat_text_element(flat: &FlatText, ix: usize, opts: &RenderOptions) -> AnyElement {
    // Streaming veil: opacity-only recolor of the runs covering newly appended
    // chunks. Same text, same fonts, same lengths — layout is untouched.
    // Settled elements return no spans and reuse the cached runs unsplit.
    let text_runs = match &opts.veil {
        Some(veil) => {
            let spans = veil.borrow_mut().advance(ix, &flat.text, opts.now);
            apply_veil(flat.runs.clone(), &spans)
        }
        None => flat.runs.clone(),
    };
    let styled = StyledText::new(flat.text.clone()).with_runs(text_runs);
    let layout = styled.layout().clone();
    let text_el: AnyElement = if flat.links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = flat.links.iter().cloned().unzip();
        let id: SharedString = format!("{}-t{ix}", opts.row_key).into();
        InteractiveText::new(id, styled)
            .on_click(ranges, move |clicked_ix, _window, cx| {
                if let Some(url) = urls.get(clicked_ix) {
                    cx.open_url(url);
                }
            })
            .into_any_element()
    };
    if flat.code_ranges.is_empty() {
        return text_el;
    }
    // Rounded inline-code washes: a canvas sibling painted BEFORE the text
    // (earlier sibling ⇒ underneath), reading glyph geometry from the text's
    // own layout handle. Pure paint — the wash never participates in layout.
    let ranges = flat.code_ranges.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            let wash = inline_code_wash();
            for range in &ranges {
                for rect in code_wash_rects(&layout, range) {
                    window.paint_quad(quad(
                        rect,
                        px(INLINE_CODE_RADIUS),
                        wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(text_el)
        .into_any_element()
}

/// The rounded-wash boxes for one inline-code byte range: one slightly inset,
/// 2px-overhanging box per visual line the range covers (soft wraps split it).
/// Window coordinates, from the laid-out text's own geometry.
fn code_wash_rects(layout: &gpui::TextLayout, range: &Range<usize>) -> Vec<Bounds<gpui::Pixels>> {
    let mut rects = Vec::new();
    let line_height = layout.line_height();
    let mut cur = range.start;
    // Walk the range one visual row at a time: find the furthest index that
    // still sits on the current row (binary search over glyph positions).
    let mut guard = 0;
    while cur < range.end && guard < 64 {
        guard += 1;
        let Some(p1) = layout.position_for_index(cur) else {
            break;
        };
        // `seg_end` closes the wash on this row; `next` is the first index on
        // the following row (strict progress even though a row-end index's
        // position still reports the earlier row).
        let (seg_end, next) = match layout.position_for_index(range.end) {
            Some(pe) if pe.y == p1.y => (range.end, range.end),
            _ => {
                // Largest ix on this row (probes stay on char boundaries only
                // at the ends; intermediate probes just need a y).
                let (mut lo, mut hi) = (cur, range.end);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    match layout.position_for_index(mid) {
                        Some(pm) if pm.y == p1.y => lo = mid,
                        _ => hi = mid,
                    }
                }
                (lo, hi)
            }
        };
        if let Some(p2) = layout.position_for_index(seg_end)
            && p2.x > p1.x
        {
            rects.push(Bounds::new(
                point(
                    p1.x - px(INLINE_CODE_PAD_X),
                    p1.y + px(INLINE_CODE_INSET_Y),
                ),
                size(
                    p2.x - p1.x + px(2.0 * INLINE_CODE_PAD_X),
                    line_height - px(2.0 * INLINE_CODE_INSET_Y),
                ),
            ));
        }
        if next <= cur {
            break;
        }
        cur = next;
    }
    rects
}

#[allow(clippy::too_many_arguments)]
fn text_element(
    runs: &[InlineRun],
    size: f32,
    line_height: f32,
    bold_default: bool,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    let weight = if bold_default {
        FontWeight::SEMIBOLD
    } else {
        FontWeight::NORMAL
    };
    let flat = flatten_cached(runs, weight, top_ix, ix, opts, theme);
    let inner = flat_text_element(&flat, ix, opts);
    div()
        .text_size(px(size))
        .line_height(px(line_height))
        .child(inner)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_code_block(
    language: Option<&str>,
    code: &str,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    highlight: CodeHighlight,
) -> AnyElement {
    let mono = font(theme.font_mono.clone());
    // Per-line strings + runs through the cross-frame cache (validity: code
    // length + highlight slice identity — a fresh highlight Arc re-derives).
    let hl_key = highlight.map_or((0, 0), |h| (h.as_ptr() as usize, h.len()));
    let build = || {
        let lines: Vec<(SharedString, Vec<TextRun>)> = code
            .split('\n')
            .enumerate()
            .map(|(li, line)| {
                let tokens = highlight
                    .and_then(|h| h.get(li))
                    .map(|t| &t[..])
                    .unwrap_or(&[]);
                (
                    SharedString::from(line.to_string()),
                    runs_for_code_line(line, tokens, &mono, theme),
                )
            })
            .collect();
        Rc::new(CachedCode {
            code_len: code.len(),
            hl_key,
            lines,
        })
    };
    let cached: Rc<CachedCode> = match &opts.cache {
        Some(cache) => {
            let mut cache = cache.borrow_mut();
            let entry = cache
                .code
                .entry((opts.row_key.clone(), top_ix, ix))
                .or_insert_with(&build);
            if entry.code_len != code.len() || entry.hl_key != hl_key {
                *entry = build();
            }
            entry.clone()
        }
        None => build(),
    };
    // Streaming veil over appended code, tracked on the whole code text and
    // sliced per line below (paint-only run recolor — heights stay exact).
    let veil_spans = match &opts.veil {
        Some(veil) => veil.borrow_mut().advance(ix, code, opts.now),
        None => Vec::new(),
    };
    let scroll_id: SharedString = format!("{}-code{ix}", opts.row_key).into();
    // Copy affordance (round 9; no source counterpart — the original block is
    // header + body only): a small ghost button in the block's top-right,
    // absolutely overlaid so clicking / the "Copied" flash never shifts
    // layout. Sits centered in the header when there is one, floats over the
    // first code line otherwise.
    let copy_button = opts.copy.clone().map(|copy| {
        let copied = copy.copied_ix == Some(ix);
        let code_text: SharedString = code.to_string().into();
        let handler = copy.handler.clone();
        div()
            .id(SharedString::from(format!("{}-copy{ix}", opts.row_key)))
            .absolute()
            .top(px(3.0))
            .right(px(5.0))
            .h(px(20.0))
            .px(px(6.0))
            .rounded(px(5.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::white_alpha(0.08)))
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .on_click(move |_, window, cx| handler(ix, code_text.clone(), window, cx))
            .child(
                crate::icons::icon(if copied {
                    crate::icons::CHECK
                } else {
                    crate::icons::COPY
                })
                .size(px(12.0))
                .text_color(theme.text_muted),
            )
            .when(copied, |el| el.child(SharedString::from("Copied")))
    });
    div()
        .rounded(px(10.0))
        // Faint white wash over the near-black panel ≈ #101010 (comet's code
        // surface), with the hairline border.
        .bg(crate::theme::white_alpha(0.035))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .relative()
        .when_some(language, |el, lang| {
            el.child(
                div()
                    .px(px(CODE_PADDING_X))
                    .py(px(5.0))
                    .border_b_1()
                    .border_color(theme.border)
                    // A whisper of tone separation between header and body.
                    .bg(crate::theme::white_alpha(0.02))
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
                .children((0..cached.lines.len()).scan(0usize, move |off, li| {
                    let (line, runs) = &cached.lines[li];
                    let start = *off;
                    *off = start + line.len() + 1; // +1 for the '\n'
                    let local = slice_spans(&veil_spans, start, start + line.len());
                    let runs = apply_veil(runs.clone(), &local);
                    Some(
                        div()
                            .h(px(CODE_LINE_HEIGHT))
                            .flex_none()
                            .child(StyledText::new(line.clone()).with_runs(runs)),
                    )
                })),
        )
        // Overlay LAST so it paints above the header/body.
        .children(copy_button)
        .into_any_element()
}

/// Paint color for a token class — the soft syntax palette (round 9: the
/// original's mdTheme code blocks are monochrome `#e7e7e7`, but the user
/// asked for color; these are the diff pane's hues, now shared by both).
pub fn token_color(class: TokenClass, theme: &Theme) -> Hsla {
    match class {
        TokenClass::Keyword => crate::theme::oklch(0.709, 0.129, 20.0), // soft rose
        TokenClass::StringLit => crate::theme::oklch(0.77, 0.11, 168.0), // soft green
        TokenClass::Number => crate::theme::oklch(0.78, 0.12, 80.0),    // soft amber
        TokenClass::Comment => theme.text_faint,
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
    runs_with_palette(line, tokens, mono, theme.text, |class| {
        token_color(class, theme)
    })
}

/// [`runs_for_code_line`] with a caller-supplied palette (the diff pane keys
/// its plain color differently; the hues are shared via [`token_color`]).
pub fn runs_with_palette(
    line: &str,
    tokens: &[Token],
    mono: &gpui::Font,
    plain_color: Hsla,
    color_for: impl Fn(TokenClass) -> Hsla,
) -> Vec<TextRun> {
    let plain = |len: usize| TextRun {
        len,
        font: mono.clone(),
        color: plain_color,
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
        run.color = color_for(token.class);
        runs.push(run);
        at = token.range.end;
    }
    if at < line.len() {
        runs.push(plain(line.len() - at));
    }
    runs.retain(|r| r.len > 0);
    runs
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
    fn flatten_collects_and_merges_inline_code_ranges() {
        let theme = Theme::dark();
        let code = |text: &str| InlineRun {
            text: text.into(),
            style: InlineStyle {
                code: true,
                ..Default::default()
            },
        };
        let plain = |text: &str| InlineRun {
            text: text.into(),
            style: InlineStyle::default(),
        };
        let flat = flatten_runs(
            &[plain("use "), code("foo"), code("()"), plain(" and "), code("bar")],
            &theme,
            false,
        );
        // Adjacent code runs merge into ONE wash box; separated ones don't.
        assert_eq!(flat.code_ranges, vec![4..9, 14..17]);
        // Code text is the violet tint; the square run background is gone
        // (the rounded wash is painted by the canvas underlay instead).
        assert_eq!(flat.runs[1].color, inline_code_text());
        assert_eq!(flat.runs[1].background_color, None);
        assert_eq!(flat.runs[0].color, theme.text);
    }

    #[test]
    fn code_palette_is_colored_and_shared() {
        // Round 9: transcript code blocks paint the soft hues (rose keyword,
        // green string, amber number); comments stay faint neutral.
        let theme = Theme::dark();
        assert_ne!(token_color(TokenClass::Keyword, &theme), theme.text);
        assert_ne!(
            token_color(TokenClass::StringLit, &theme),
            token_color(TokenClass::Keyword, &theme)
        );
        assert_eq!(token_color(TokenClass::Comment, &theme), theme.text_faint);
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
    fn table_columns_floor_and_padding() {
        // A short column keeps its content width (floored at MIN_COLUMN_CONTENT
        // + padding); a wide one may wrap but no narrower than minColumnWidth.
        let geo = table_columns(&[10.0, 200.0]);
        assert_eq!(geo.naturals, vec![72.0, 224.0]); // 48+24, 200+24
        assert_eq!(geo.minimums, vec![72.0, 96.0]);
        assert_eq!(geo.min_table_width, 168.0);
    }

    #[test]
    fn table_columns_are_content_proportional_not_equal() {
        let geo = table_columns(&[300.0, 60.0, 60.0]);
        // Flex grow factors are the naturals — a prose column gets a larger
        // share than short ones (not equal thirds).
        assert!(geo.naturals[0] > 3.0 * geo.naturals[1] * 0.9);
        assert_eq!(geo.naturals[1], geo.naturals[2]);
    }

    #[test]
    fn table_header_flattens_at_weight_700() {
        let theme = Theme::dark();
        let runs = vec![InlineRun {
            text: "Header".into(),
            style: InlineStyle::default(),
        }];
        let flat = flatten_runs_weighted(&runs, &theme, TABLE_HEADER_WEIGHT);
        assert_eq!(flat.runs[0].font.weight, FontWeight::BOLD);
        // Strong runs inside a 700 header stay 700 (never drop to semibold).
        let bold_runs = vec![InlineRun {
            text: "Strong".into(),
            style: InlineStyle {
                bold: true,
                ..Default::default()
            },
        }];
        let flat = flatten_runs_weighted(&bold_runs, &theme, TABLE_HEADER_WEIGHT);
        assert_eq!(flat.runs[0].font.weight, FontWeight::BOLD);
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
