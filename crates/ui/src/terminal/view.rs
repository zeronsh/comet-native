//! Terminal paint + input encoding.
//!
//! - theme-owned background/ANSI16 plus the xterm 256-color cube/grayscale;
//! - keystroke → PTY byte encoding (printables, control keys, arrows/nav
//!   escape sequences, Ctrl- combos, Alt prefixing);
//! - the 12 ms input coalescer and the 80 ms resize debounce constants (the
//!   panel drives the timers; the buffer logic here is pure);
//! - [`TerminalElement`] — a custom gpui element that measures cell metrics
//!   from the real mono font (the "font probe"), reports the resulting
//!   cols×rows back to the panel, and paints the grid: background quads for
//!   non-default cells, one `ShapedLine` per row (same font whatever the
//!   colors — paint never changes layout), and the cursor block.

use gpui::{
    App, Bounds, Entity, GlobalElementId, Hsla, LayoutId, Modifiers, PaintQuad, Pixels, ShapedLine,
    SharedString, Style, TextRun, Window, fill, font, outline, point, px, relative, size,
};

use crate::theme::{Appearance, Theme, rgb_to_hsl};

use super::emulator::{CellColor, CellSnapshot, Side};
use super::panel::TerminalPanel;

/// Terminal font metrics (mono).
pub const TERM_FONT_SIZE: f32 = 13.0;
pub const TERM_LINE_HEIGHT: f32 = 18.0;
/// Inner padding of the grid area.
pub const TERM_PADDING: f32 = 12.0;

/// Keyboard input coalescing window before a `WriteTerminal` flush.
pub const COALESCE_MS: u64 = 12;
/// Debounce for `ResizeTerminal` after viewport-driven size changes.
pub const RESIZE_DEBOUNCE_MS: u64 = 80;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// The panel fill behind the grid. On glass the opaque tone thins to a
/// translucent wash so the blurred desktop reads through like the rest of the
/// chrome (same move as [`Theme::card_glass_bg`]); opaque platforms keep the
/// true tone. Explicit cell backgrounds (vim colorschemes etc.) still paint
/// their own opaque quads on top.
pub fn terminal_panel_bg(theme: &Theme) -> Hsla {
    if theme.is_glass() {
        theme.terminal.background.opacity(0.4)
    } else {
        theme.terminal.background
    }
}

fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    let (h, s, l) = rgb_to_hsl(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    gpui::hsla(h, s, l, 1.0)
}

/// xterm 256-color cube component levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve the xterm extended-color range to RGB components.
///
/// Three ranges, treated differently on purpose:
///
/// - **0-15** are named slots and are resolved directly from `Theme::terminal`.
/// - **16-231** is the 6×6×6 cube: a program asking for index 196 is asking for
///   `#ff0000` by arithmetic, and remapping it would be inventing colors the
///   caller did not pick. Left alone in both appearances, same as iTerm/Apple
///   Terminal light themes.
/// - **232-255** is the grayscale ramp, which tools use for *de-emphasis*
///   rather than for a specific grey. Its dark→light direction only reads as
///   "dim" on a dark background, so light mode mirrors the ramp: index 232
///   stays the faintest and 255 the strongest in both. Without this the ramp is
///   the single biggest legibility hole on white, because its bright end —
///   where most "dim hint" text lands — is the end that vanishes.
fn extended_indexed_rgb(appearance: Appearance, index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => unreachable!("ANSI16 is resolved from the active theme"),
        16..=231 => {
            let n = index as usize - 16;
            (
                CUBE_LEVELS[n / 36],
                CUBE_LEVELS[(n / 6) % 6],
                CUBE_LEVELS[n % 6],
            )
        }
        232..=255 => {
            let step = index - 232;
            let step = match appearance {
                Appearance::Dark => step,
                Appearance::Light => 23 - step,
            };
            let v = 8 + 10 * step;
            (v, v, v)
        }
    }
}

/// Resolve a cell color to paint against the theme.
pub fn resolve_color(color: CellColor, theme: &Theme) -> Hsla {
    match color {
        CellColor::Foreground => theme.terminal.foreground,
        CellColor::Background => theme.terminal.background,
        CellColor::Indexed(ix @ 0..=15) => theme.terminal.ansi[ix as usize],
        CellColor::Indexed(ix) => {
            let (r, g, b) = extended_indexed_rgb(theme.appearance, ix);
            rgb8(r, g, b)
        }
        CellColor::Rgb(r, g, b) => rgb8(r, g, b),
    }
}

// ---------------------------------------------------------------------------
// Pointer → cell
// ---------------------------------------------------------------------------

/// Minimum pointer travel before a press turns into a selection.
///
/// Without it, the click that focuses the panel starts a one-cell selection if
/// the hand moves a pixel — and once anything copies on selection change, that
/// silently clobbers the clipboard. Matches the threshold zed uses for the same
/// reason (`SELECTION_DRAG_THRESHOLD`), which is gpui's own `div` drag
/// threshold.
pub const SELECTION_DRAG_THRESHOLD: f32 = 2.0;

/// Which cell a pointer landed on, and which edge of it a selection anchors to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellHit {
    pub row: usize,
    pub col: usize,
    /// Selections anchor to a cell *edge*, not a cell: pressing on the left
    /// half of a glyph includes it, the right half excludes it.
    pub side: Side,
}

/// Map a position *relative to the grid's top-left glyph* onto a cell.
///
/// Positions outside the grid clamp to the nearest cell rather than returning
/// `None`, because that is what a drag needs: the pointer routinely leaves the
/// panel mid-gesture, and the selection should extend to the edge it left
/// through instead of freezing at the last sample taken inside.
///
/// Overshoot also *forces the side*, which clamping alone does not give you.
/// Dragging past the bottom should take the last line whole, even when the
/// pointer drifted left of where it started — deriving the side from x there
/// would stop the selection mid-row. Same rule going up, mirrored. This is the
/// behaviour alacritty and zed's `grid_point_and_side` both implement.
pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: usize, rows: usize) -> CellHit {
    // Degenerate metrics (a zero-size grid, or a font probe that returned NaN)
    // would otherwise divide into garbage cell indices.
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if cols == 0 || rows == 0 || !usable(cell_w) || !usable(line_h) {
        return CellHit {
            row: 0,
            col: 0,
            side: Side::Left,
        };
    }
    let x = if x.is_finite() { x } else { 0.0 };
    let y = if y.is_finite() { y } else { 0.0 };
    let last_col = cols - 1;
    let last_row = rows - 1;

    let raw_col = (x / cell_w).floor();
    let mut side = if x.max(0.0) % cell_w > cell_w / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    let col = if raw_col > last_col as f32 {
        side = Side::Right;
        last_col
    } else {
        raw_col.max(0.0) as usize
    };

    let raw_row = (y / line_h).floor();
    let row = if raw_row > last_row as f32 {
        side = Side::Right;
        last_row
    } else if raw_row < 0.0 {
        side = Side::Left;
        0
    } else {
        raw_row as usize
    };

    CellHit { row, col, side }
}

// ---------------------------------------------------------------------------
// Keyboard → bytes
// ---------------------------------------------------------------------------

/// Encode a keystroke as PTY bytes. `None` means "not ours" — the event should
/// fall through (e.g. the platform-primary shortcuts that drive app actions).
///
/// `app_cursor` switches arrows/home/end from CSI to SS3 per DECCKM.
pub fn keystroke_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: &Modifiers,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // Platform-primary combos (Cmd on macOS, the super key elsewhere) belong to
    // the app keymap, never the PTY.
    if mods.platform {
        return None;
    }
    if mods.alt {
        // ESC-prefix the same keystroke without alt.
        let inner = keystroke_bytes(
            key,
            key_char,
            &Modifiers {
                alt: false,
                ..*mods
            },
            app_cursor,
        )?;
        let mut out = vec![0x1b];
        out.extend(inner);
        return Some(out);
    }
    if mods.control {
        return control_bytes(key);
    }

    let seq = |csi: &[u8], ss3: &[u8]| {
        Some(if app_cursor {
            ss3.to_vec()
        } else {
            csi.to_vec()
        })
    };
    match key {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "tab" => Some(if mods.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        "escape" => Some(vec![0x1b]),
        "space" => Some(b" ".to_vec()),
        "up" => seq(b"\x1b[A", b"\x1bOA"),
        "down" => seq(b"\x1b[B", b"\x1bOB"),
        "right" => seq(b"\x1b[C", b"\x1bOC"),
        "left" => seq(b"\x1b[D", b"\x1bOD"),
        "home" => seq(b"\x1b[H", b"\x1bOH"),
        "end" => seq(b"\x1b[F", b"\x1bOF"),
        "insert" => Some(b"\x1b[2~".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "f1" => Some(b"\x1bOP".to_vec()),
        "f2" => Some(b"\x1bOQ".to_vec()),
        "f3" => Some(b"\x1bOR".to_vec()),
        "f4" => Some(b"\x1bOS".to_vec()),
        "f5" => Some(b"\x1b[15~".to_vec()),
        "f6" => Some(b"\x1b[17~".to_vec()),
        "f7" => Some(b"\x1b[18~".to_vec()),
        "f8" => Some(b"\x1b[19~".to_vec()),
        "f9" => Some(b"\x1b[20~".to_vec()),
        "f10" => Some(b"\x1b[21~".to_vec()),
        "f11" => Some(b"\x1b[23~".to_vec()),
        "f12" => Some(b"\x1b[24~".to_vec()),
        _ => {
            // Printable: prefer the typed character (IME/shift-aware).
            let text = key_char.filter(|c| !c.is_empty()).or({
                // Fall back to single-char key names ("a", "/", …).
                if key.chars().count() == 1 {
                    Some(key)
                } else {
                    None
                }
            })?;
            Some(text.as_bytes().to_vec())
        }
    }
}

/// Ctrl-key encoding (caret notation).
fn control_bytes(key: &str) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    let (c, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return match key {
            "space" => Some(vec![0x00]),
            "backspace" => Some(vec![0x08]),
            "enter" => Some(b"\r".to_vec()),
            _ => None,
        };
    }
    match c {
        'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
        '@' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' | '/' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

/// Wrap pasted text for the PTY (bracketed-paste aware; strips the one control
/// sequence a paste could inject).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let sanitized = text.replace("\x1b[201~", "");
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

// ---------------------------------------------------------------------------
// Input coalescer (pure buffer; the panel owns the 12 ms timer)
// ---------------------------------------------------------------------------

/// Buffers keyboard bytes between flushes. `push` returns `true` exactly when
/// a flush timer should be scheduled (the buffer was empty), so at most one
/// timer is in flight per burst.
#[derive(Debug, Default)]
pub struct InputCoalescer {
    pending: Vec<u8>,
}

impl InputCoalescer {
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        let was_empty = self.pending.is_empty();
        self.pending.extend_from_slice(bytes);
        was_empty && !self.pending.is_empty()
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Grid element
// ---------------------------------------------------------------------------

/// Paints the active tab's grid. Cell metrics come from the resolved mono font
/// each frame (font probe): `em_advance` for the cell width, the fixed line
/// height for rows. The measured cols×rows feed back into the panel, which
/// resizes the emulator immediately and debounces the `ResizeTerminal` RPC.
pub struct TerminalElement {
    panel: Entity<TerminalPanel>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(panel: Entity<TerminalPanel>, focused: bool) -> Self {
        Self { panel, focused }
    }
}

pub struct TerminalPrepaint {
    bg_quads: Vec<PaintQuad>,
    /// Selection wash. Painted after [`Self::bg_quads`] and before the glyphs:
    /// it has to tint a cell's own background rather than replace it, and it
    /// must not wash out the text it is highlighting.
    sel_quads: Vec<PaintQuad>,
    /// Per row, the shaped segments and the grid COLUMN each one starts at.
    /// Not one line per row: see [`shape_row`].
    lines: Vec<Vec<(usize, ShapedLine)>>,
    /// Grid cell advance, so paint can place segments by column.
    cell_w: Pixels,
    cursor: Option<PaintQuad>,
}

impl gpui::IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = Theme::of(cx).clone();
        // Ligatures OFF. A terminal is a fixed grid: the shaper must emit one
        // cell-width advance per character, and a contextual substitution
        // (Geist Mono ligates `--`, `->`, …) collapses several cells into
        // fewer glyphs, so the row renders SHORT while the cursor — a quad at
        // `cell_w * col` — stays on the true column. That is the `codex
        // --yolo` → `codex--yolo` report: the space is in the grid and went
        // to the pty (the command runs), only the painted run lost a cell.
        // The landing page disables the same three features on its ASCII art
        // for the same reason.
        let mut mono = font(theme.font_mono.clone());
        mono.features = gpui::FontFeatures(std::sync::Arc::new(vec![
            ("liga".into(), 0),
            ("calt".into(), 0),
            ("dlig".into(), 0),
        ]));
        // Font probe: measure the actual advance of the resolved mono font so
        // cols/rows track real glyph metrics, not a guessed aspect ratio.
        let font_size = px(TERM_FONT_SIZE);
        let font_id = window.text_system().resolve_font(&mono);
        let cell_w = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(px(TERM_FONT_SIZE * 0.6));
        let line_h = px(TERM_LINE_HEIGHT);

        let inner_w = f32::from(bounds.size.width) - 2.0 * TERM_PADDING;
        let inner_h = f32::from(bounds.size.height) - 2.0 * TERM_PADDING;
        let cols = ((inner_w / f32::from(cell_w)).floor() as i64).clamp(2, 500) as u16;
        let rows = ((inner_h / f32::from(line_h)).floor() as i64).clamp(1, 500) as u16;

        // Report the measured grid, then snapshot for painting. Safe: the
        // panel entity is not borrowed during element prepaint.
        let origin = point(
            bounds.left() + px(TERM_PADDING),
            bounds.top() + px(TERM_PADDING),
        );
        let snapshot = self.panel.update(cx, |panel, cx| {
            panel.on_grid_metrics(
                super::panel::GridGeometry {
                    bounds,
                    origin,
                    cell_w: f32::from(cell_w),
                    line_h: f32::from(line_h),
                    cols,
                    rows,
                },
                cx,
            );
            panel.active_grid_snapshot(cx)
        });
        let Some(snapshot) = snapshot else {
            return TerminalPrepaint {
                bg_quads: Vec::new(),
                sel_quads: Vec::new(),
                lines: Vec::new(),
                cell_w,
                cursor: None,
            };
        };

        let mut bg_quads = Vec::new();
        let mut sel_quads = Vec::new();
        let mut lines = Vec::with_capacity(snapshot.lines.len());

        for (row_ix, row) in snapshot.lines.iter().enumerate() {
            let y = origin.y + line_h * row_ix as f32;
            // Selected runs, merged the same way background runs are: one quad
            // per contiguous span instead of one per cell.
            let mut sel_start: Option<usize> = None;
            for col in 0..=row.len() {
                let selected = row.get(col).is_some_and(|cell| cell.selected);
                match (sel_start, selected) {
                    (None, true) => sel_start = Some(col),
                    (Some(start), false) => {
                        sel_quads.push(fill(
                            Bounds::new(
                                point(origin.x + cell_w * start as f32, y),
                                size(cell_w * (col - start) as f32, line_h),
                            ),
                            theme.terminal.selection,
                        ));
                        sel_start = None;
                    }
                    _ => {}
                }
            }
            // Merge consecutive non-default background cells into quads.
            let mut run_start: Option<(usize, Hsla)> = None;
            for (col, color) in row
                .iter()
                .map(|cell| cell.display_colors().1)
                .chain(std::iter::once(CellColor::Background))
                .enumerate()
            {
                let paint = match color {
                    CellColor::Background => None,
                    other => Some(resolve_color(other, &theme)),
                };
                match (&run_start, paint) {
                    (None, Some(color)) => run_start = Some((col, color)),
                    (Some((start, current)), next) if next != Some(*current) => {
                        bg_quads.push(fill(
                            Bounds::new(
                                point(origin.x + cell_w * *start as f32, y),
                                size(cell_w * (col - *start) as f32, line_h),
                            ),
                            *current,
                        ));
                        run_start = next.map(|color| (col, color));
                    }
                    _ => {}
                }
            }
            lines.push(shape_row(row, &theme, &mono, font_size, window));
        }

        let cursor = snapshot.cursor.map(|c| {
            let cursor_bounds = Bounds::new(
                point(
                    origin.x + cell_w * c.col as f32,
                    origin.y + line_h * c.row as f32,
                ),
                size(cell_w, line_h),
            );
            if self.focused {
                // Translucent block: the glyph underneath stays legible.
                fill(cursor_bounds, theme.cursor)
            } else {
                outline(cursor_bounds, theme.cursor, gpui::BorderStyle::Solid)
            }
        });

        TerminalPrepaint {
            bg_quads,
            sel_quads,
            lines,
            cell_w,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line_h = px(TERM_LINE_HEIGHT);
        let origin = point(
            bounds.left() + px(TERM_PADDING),
            bounds.top() + px(TERM_PADDING),
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.bg_quads.drain(..) {
                window.paint_quad(quad);
            }
            for quad in prepaint.sel_quads.drain(..) {
                window.paint_quad(quad);
            }
            let cell_w = prepaint.cell_w;
            for (ix, segments) in prepaint.lines.iter().enumerate() {
                let y = origin.y + line_h * ix as f32;
                for (col, line) in segments {
                    let _ = line.paint(
                        point(origin.x + cell_w * *col as f32, y),
                        line_h,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
    }
}

/// Shape one grid row into COLUMN-PINNED segments.
///
/// A terminal is a fixed grid, but a shaped line places glyphs by their font
/// advances. Those agree only while every glyph is monospace-width — the row's
/// `cell_w` IS the mono font's em advance. The moment a glyph resolves through
/// FONT FALLBACK (box drawing `│─╭`, arrows `→`, emoji, CJK) its advance is
/// whatever that other font uses, and the whole rest of the line slides out of
/// the grid: box borders land a few pixels off (user report: "one of the pipes
/// is broken"), and a double-width glyph whose fallback advances only one cell
/// swallows the column after it (user report: `codex --yolo` rendering as
/// `codex--yolo`). Backgrounds, selection and the cursor never drifted because
/// those are quads placed at `cell_w * col`.
///
/// So: runs of ASCII shape together (guaranteed cell-width in a mono font),
/// and every other glyph is its own segment pinned at its own column. Wide
/// spacers are still skipped — the wide glyph covers both columns, and the
/// NEXT segment re-pins regardless.
fn shape_row(
    row: &[CellSnapshot],
    theme: &Theme,
    mono: &gpui::Font,
    font_size: Pixels,
    window: &Window,
) -> Vec<(usize, ShapedLine)> {
    fn flush(
        segments: &mut Vec<(usize, ShapedLine)>,
        text: &mut String,
        runs: &mut Vec<TextRun>,
        seg_col: usize,
        font_size: Pixels,
        window: &Window,
    ) {
        if text.is_empty() {
            return;
        }
        let shaped = window.text_system().shape_line(
            SharedString::from(std::mem::take(text)),
            font_size,
            runs,
            None,
        );
        segments.push((seg_col, shaped));
        runs.clear();
    }

    let mut segments: Vec<(usize, ShapedLine)> = Vec::new();
    let mut text = String::with_capacity(row.len());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut seg_col = 0usize;

    for (col, cell) in row.iter().enumerate() {
        if cell.wide_spacer {
            continue;
        }
        let ch = if cell.hidden { ' ' } else { cell.ch };
        // Anything that can leave the mono font gets its own pinned segment.
        let pinned = !ch.is_ascii() || cell.wide;
        if pinned {
            flush(
                &mut segments,
                &mut text,
                &mut runs,
                seg_col,
                font_size,
                window,
            );
        }
        if text.is_empty() {
            seg_col = col;
        }
        let (fg, _) = cell.display_colors();
        let mut color = resolve_color(fg, theme);
        if cell.dim {
            color.a *= 0.6;
        }
        let mut cell_font = mono.clone();
        cell_font.weight = if cell.bold {
            gpui::FontWeight::BOLD
        } else {
            gpui::FontWeight::NORMAL
        };
        cell_font.style = if cell.italic {
            gpui::FontStyle::Italic
        } else {
            gpui::FontStyle::Normal
        };
        let underline = cell.underline.then_some(gpui::UnderlineStyle {
            color: Some(color),
            thickness: px(1.0),
            wavy: false,
        });
        let len = ch.len_utf8();
        text.push(ch);
        match runs.last_mut() {
            Some(last)
                if last.color == color && last.font == cell_font && last.underline == underline =>
            {
                last.len += len;
            }
            _ => runs.push(TextRun {
                len,
                font: cell_font,
                color,
                background_color: None,
                underline,
                strikethrough: None,
            }),
        }
        if pinned {
            flush(
                &mut segments,
                &mut text,
                &mut runs,
                seg_col,
                font_size,
                window,
            );
        }
    }
    flush(
        &mut segments,
        &mut text,
        &mut runs,
        seg_col,
        font_size,
        window,
    );
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn printables_prefer_key_char() {
        assert_eq!(
            keystroke_bytes("a", Some("a"), &mods(), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            keystroke_bytes(
                "a",
                Some("A"),
                &Modifiers {
                    shift: true,
                    ..mods()
                },
                false
            ),
            Some(b"A".to_vec())
        );
        // Multi-byte characters pass through as UTF-8.
        assert_eq!(
            keystroke_bytes("e", Some("é"), &mods(), false),
            Some("é".as_bytes().to_vec())
        );
        // Named single-char keys fall back to the key name.
        assert_eq!(
            keystroke_bytes("/", None, &mods(), false),
            Some(b"/".to_vec())
        );
        // Unknown multi-char keys are not ours.
        assert_eq!(keystroke_bytes("capslock", None, &mods(), false), None);
    }

    #[test]
    fn control_keys_and_sequences() {
        assert_eq!(
            keystroke_bytes("enter", None, &mods(), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_bytes("backspace", None, &mods(), false),
            Some(vec![0x7f])
        );
        assert_eq!(
            keystroke_bytes("tab", None, &mods(), false),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_bytes(
                "tab",
                None,
                &Modifiers {
                    shift: true,
                    ..mods()
                },
                false
            ),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            keystroke_bytes("escape", None, &mods(), false),
            Some(vec![0x1b])
        );
        assert_eq!(
            keystroke_bytes("delete", None, &mods(), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            keystroke_bytes("pageup", None, &mods(), false),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            keystroke_bytes("f5", None, &mods(), false),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        assert_eq!(
            keystroke_bytes("up", None, &mods(), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_bytes("up", None, &mods(), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            keystroke_bytes("home", None, &mods(), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            keystroke_bytes("end", None, &mods(), true),
            Some(b"\x1bOF".to_vec())
        );
    }

    #[test]
    fn ctrl_combos_map_to_control_bytes() {
        let ctrl = Modifiers {
            control: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", Some("c"), &ctrl, false),
            Some(vec![0x03])
        );
        assert_eq!(keystroke_bytes("z", None, &ctrl, false), Some(vec![0x1a]));
        assert_eq!(
            keystroke_bytes("space", None, &ctrl, false),
            Some(vec![0x00])
        );
        assert_eq!(keystroke_bytes("[", None, &ctrl, false), Some(vec![0x1b]));
        assert_eq!(keystroke_bytes("_", None, &ctrl, false), Some(vec![0x1f]));
        // Ctrl+1 has no caret encoding — not ours.
        assert_eq!(keystroke_bytes("1", Some("1"), &ctrl, false), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Modifiers {
            alt: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("b", Some("b"), &alt, false),
            Some(vec![0x1b, b'b'])
        );
        let alt_ctrl = Modifiers {
            alt: true,
            control: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", None, &alt_ctrl, false),
            Some(vec![0x1b, 0x03])
        );
    }

    #[test]
    fn platform_primary_combos_fall_through() {
        let cmd = Modifiers {
            platform: true,
            ..mods()
        };
        assert_eq!(keystroke_bytes("j", Some("j"), &cmd, false), None);
        assert_eq!(keystroke_bytes("enter", None, &cmd, false), None);
    }

    #[test]
    fn paste_wraps_when_bracketed() {
        assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        // Close-bracket injection is stripped.
        assert_eq!(
            paste_bytes("a\x1b[201~rm -rf", true),
            b"\x1b[200~arm -rf\x1b[201~".to_vec()
        );
    }

    #[test]
    fn coalescer_schedules_once_per_burst() {
        let mut c = InputCoalescer::default();
        assert!(c.is_empty());
        assert!(c.push(b"a"), "first push schedules the flush");
        assert!(!c.push(b"b"), "subsequent pushes ride the pending flush");
        assert!(!c.push(b"c"));
        assert_eq!(c.take(), b"abc".to_vec());
        assert!(c.is_empty());
        // Next burst schedules again.
        assert!(c.push(b"d"));
        // Empty pushes never schedule.
        let mut c = InputCoalescer::default();
        assert!(!c.push(b""));
    }

    #[test]
    fn cube_is_appearance_independent() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            // 16 = cube origin (0,0,0); 231 = cube max (255,255,255).
            assert_eq!(extended_indexed_rgb(appearance, 16), (0, 0, 0));
            assert_eq!(extended_indexed_rgb(appearance, 231), (255, 255, 255));
            // 196 = pure red corner: 16 + 36*5.
            assert_eq!(extended_indexed_rgb(appearance, 196), (255, 0, 0));
            // 21 = pure blue corner.
            assert_eq!(extended_indexed_rgb(appearance, 21), (0, 0, 255));
        }
    }

    #[test]
    fn grayscale_ramp_mirrors_in_light() {
        // Dark: 232 → 8 (faintest), 255 → 238 (strongest).
        assert_eq!(extended_indexed_rgb(Appearance::Dark, 232), (8, 8, 8));
        assert_eq!(extended_indexed_rgb(Appearance::Dark, 255), (238, 238, 238));
        // Light reverses it so the faint end stays faint against white.
        assert_eq!(
            extended_indexed_rgb(Appearance::Light, 232),
            (238, 238, 238)
        );
        assert_eq!(extended_indexed_rgb(Appearance::Light, 255), (8, 8, 8));
    }

    #[test]
    fn registered_theme_owns_terminal_background_foreground_selection_and_ansi() {
        let theme = Theme::for_selection(
            Appearance::Dark,
            "dracula",
            zeron_theme::AccentSelection::ThemeDefault,
            zeron_theme::SurfacePreference::ThemeDefault,
        );
        assert_eq!(terminal_panel_bg(&theme), theme.terminal.background);
        assert_eq!(
            resolve_color(CellColor::Foreground, &theme),
            theme.terminal.foreground
        );
        assert_eq!(
            resolve_color(CellColor::Background, &theme),
            theme.terminal.background
        );
        assert_eq!(
            resolve_color(CellColor::Indexed(1), &theme),
            theme.terminal.ansi[1]
        );
        assert!(theme.terminal.selection.a > 0.0 && theme.terminal.selection.a < 0.5);
    }

    #[test]
    fn every_registered_variant_resolves_all_ansi_slots_from_its_theme() {
        for variant in zeron_theme::ThemeRegistry::builtin()
            .families
            .iter()
            .flat_map(|family| &family.variants)
        {
            let appearance = match variant.appearance {
                zeron_theme::Appearance::Dark => Appearance::Dark,
                zeron_theme::Appearance::Light => Appearance::Light,
            };
            let theme = Theme::for_selection(
                appearance,
                &variant.id,
                zeron_theme::AccentSelection::ThemeDefault,
                zeron_theme::SurfacePreference::ThemeDefault,
            );
            for index in 0..16 {
                assert_eq!(
                    resolve_color(CellColor::Indexed(index), &theme),
                    theme.terminal.ansi[index as usize],
                    "{} ANSI {index}",
                    variant.id
                );
            }
        }
    }

    #[test]
    fn timing_constants_match_spec() {
        assert_eq!(COALESCE_MS, 12);
        assert_eq!(RESIZE_DEBOUNCE_MS, 80);
    }

    // ---- pointer → cell ----

    /// 10x20 cells, an 8x4 grid: cols 0..7, rows 0..3.
    fn hit(x: f32, y: f32) -> CellHit {
        cell_at(x, y, 10.0, 20.0, 8, 4)
    }

    #[test]
    fn pointer_maps_to_the_cell_it_is_over() {
        assert_eq!(
            hit(0.0, 0.0),
            CellHit {
                row: 0,
                col: 0,
                side: Side::Left
            }
        );
        assert_eq!(
            hit(25.0, 45.0),
            CellHit {
                row: 2,
                col: 2,
                side: Side::Left
            }
        );
        // Last cell, exactly.
        assert_eq!(
            hit(70.0, 60.0),
            CellHit {
                row: 3,
                col: 7,
                side: Side::Left
            }
        );
    }

    #[test]
    fn side_splits_the_cell_at_its_midpoint() {
        // Cell 2 spans x 20..30, so the midpoint is 25.
        assert_eq!(hit(21.0, 0.0).side, Side::Left);
        assert_eq!(
            hit(25.0, 0.0).side,
            Side::Left,
            "the midpoint itself is left"
        );
        assert_eq!(hit(26.0, 0.0).side, Side::Right);
        // The cell is unaffected by which half.
        assert_eq!(hit(21.0, 0.0).col, 2);
        assert_eq!(hit(29.0, 0.0).col, 2);
    }

    /// Dragging out of the panel must extend to the edge it left through, not
    /// freeze at the last sample inside.
    #[test]
    fn overshoot_clamps_into_the_grid() {
        assert_eq!(hit(9_999.0, 0.0).col, 7);
        assert_eq!(hit(0.0, 9_999.0).row, 3);
        assert_eq!(hit(-50.0, 0.0).col, 0);
        assert_eq!(hit(0.0, -50.0).row, 0);
    }

    /// The part clamping alone does not give you: past the right or bottom
    /// edge the side is forced Right so the last cell is *included*, and above
    /// the top it is forced Left. Dragging below-and-left must still take the
    /// bottom row whole.
    #[test]
    fn overshoot_forces_the_side_to_the_edge() {
        assert_eq!(hit(9_999.0, 10.0).side, Side::Right);
        // x sits in cell 0's left half, but the row overshot — Right wins.
        assert_eq!(hit(1.0, 9_999.0).side, Side::Right);
        assert_eq!(hit(1.0, 9_999.0).col, 0);
        // Above the top, mirrored.
        assert_eq!(hit(75.0, -50.0).side, Side::Left);
    }

    #[test]
    fn degenerate_metrics_do_not_panic() {
        assert_eq!(
            cell_at(5.0, 5.0, 0.0, 20.0, 8, 4),
            CellHit {
                row: 0,
                col: 0,
                side: Side::Left
            }
        );
        assert_eq!(
            cell_at(5.0, 5.0, 10.0, 20.0, 0, 0),
            CellHit {
                row: 0,
                col: 0,
                side: Side::Left
            }
        );
        assert_eq!(cell_at(f32::NAN, f32::INFINITY, 10.0, 20.0, 8, 4).col, 0);
    }

    #[test]
    fn drag_threshold_matches_the_gpui_default() {
        assert_eq!(SELECTION_DRAG_THRESHOLD, 2.0);
    }
}
