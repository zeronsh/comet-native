//! Terminal paint + input encoding.
//!
//! - the ANSI palette on the `#090909` terminal background (feature-inventory
//!   §1.10) and the 256-color cube/grayscale resolution;
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

use crate::theme::{Theme, rgb_to_hsl};

use super::emulator::{CellColor, CellSnapshot};
use super::panel::TerminalPanel;

/// Terminal font metrics (mono).
pub const TERM_FONT_SIZE: f32 = 13.0;
pub const TERM_LINE_HEIGHT: f32 = 18.0;
/// Inner padding of the grid area.
pub const TERM_PADDING: f32 = 8.0;

/// Keyboard input coalescing window before a `WriteTerminal` flush.
pub const COALESCE_MS: u64 = 12;
/// Debounce for `ResizeTerminal` after viewport-driven size changes.
pub const RESIZE_DEBOUNCE_MS: u64 = 80;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Terminal background — `#090909` (one step below the app's `#0a0a0a`).
pub fn terminal_bg() -> Hsla {
    rgb8(0x09, 0x09, 0x09)
}

/// The 16 ANSI colors tuned for the near-black background (indexes 0-7 normal,
/// 8-15 bright).
const ANSI16: [(u8, u8, u8); 16] = [
    (0x24, 0x24, 0x24), // black — visible against #090909
    (0xf8, 0x71, 0x71), // red
    (0x4a, 0xde, 0x80), // green
    (0xfa, 0xcc, 0x15), // yellow
    (0x60, 0xa5, 0xfa), // blue
    (0xc0, 0x84, 0xfc), // magenta
    (0x22, 0xd3, 0xee), // cyan
    (0xd4, 0xd4, 0xd8), // white
    (0x52, 0x52, 0x5b), // bright black
    (0xfc, 0xa5, 0xa5), // bright red
    (0x86, 0xef, 0xac), // bright green
    (0xfd, 0xe0, 0x47), // bright yellow
    (0x93, 0xc5, 0xfd), // bright blue
    (0xd8, 0xb4, 0xfe), // bright magenta
    (0x67, 0xe8, 0xf9), // bright cyan
    (0xfa, 0xfa, 0xfa), // bright white
];

fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    let (h, s, l) = rgb_to_hsl(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    gpui::hsla(h, s, l, 1.0)
}

/// xterm 256-color cube component levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve an indexed color (0-255) to RGB components.
pub fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI16[index as usize],
        16..=231 => {
            let n = index as usize - 16;
            (
                CUBE_LEVELS[n / 36],
                CUBE_LEVELS[(n / 6) % 6],
                CUBE_LEVELS[n % 6],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            (v, v, v)
        }
    }
}

/// Resolve a cell color to paint against the theme.
pub fn resolve_color(color: CellColor, theme: &Theme) -> Hsla {
    match color {
        CellColor::Foreground => theme.text,
        CellColor::Background => terminal_bg(),
        CellColor::Indexed(ix) => {
            let (r, g, b) = indexed_rgb(ix);
            rgb8(r, g, b)
        }
        CellColor::Rgb(r, g, b) => rgb8(r, g, b),
    }
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
    lines: Vec<ShapedLine>,
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
        let mono = font(theme.font_mono.clone());
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
        let snapshot = self.panel.update(cx, |panel, cx| {
            panel.on_grid_metrics(cols, rows, cx);
            panel.active_grid_snapshot(cx)
        });
        let Some(snapshot) = snapshot else {
            return TerminalPrepaint {
                bg_quads: Vec::new(),
                lines: Vec::new(),
                cursor: None,
            };
        };

        let origin = point(
            bounds.left() + px(TERM_PADDING),
            bounds.top() + px(TERM_PADDING),
        );
        let mut bg_quads = Vec::new();
        let mut lines = Vec::with_capacity(snapshot.lines.len());

        for (row_ix, row) in snapshot.lines.iter().enumerate() {
            let y = origin.y + line_h * row_ix as f32;
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
                fill(cursor_bounds, gpui::hsla(0.0, 0.0, 1.0, 0.35))
            } else {
                outline(
                    cursor_bounds,
                    gpui::hsla(0.0, 0.0, 1.0, 0.35),
                    gpui::BorderStyle::Solid,
                )
            }
        });

        TerminalPrepaint {
            bg_quads,
            lines,
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
            for (ix, line) in prepaint.lines.iter().enumerate() {
                let _ = line.paint(
                    point(origin.x, origin.y + line_h * ix as f32),
                    line_h,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
    }
}

/// Shape one grid row: wide-char spacers are skipped (the wide glyph covers
/// both columns), attributes map to font weight/style, colors to run colors.
fn shape_row(
    row: &[CellSnapshot],
    theme: &Theme,
    mono: &gpui::Font,
    font_size: Pixels,
    window: &Window,
) -> ShapedLine {
    let mut text = String::with_capacity(row.len());
    let mut runs: Vec<TextRun> = Vec::new();
    for cell in row {
        if cell.wide_spacer {
            continue;
        }
        let ch = if cell.hidden { ' ' } else { cell.ch };
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
    }
    window
        .text_system()
        .shape_line(SharedString::from(text), font_size, &runs, None)
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
    fn cube_and_grayscale_resolution() {
        // 16 = cube origin (0,0,0); 231 = cube max (255,255,255).
        assert_eq!(indexed_rgb(16), (0, 0, 0));
        assert_eq!(indexed_rgb(231), (255, 255, 255));
        // 196 = pure red corner: 16 + 36*5.
        assert_eq!(indexed_rgb(196), (255, 0, 0));
        // 21 = pure blue corner.
        assert_eq!(indexed_rgb(21), (0, 0, 255));
        // Grayscale ramp: 232 → 8, 255 → 238.
        assert_eq!(indexed_rgb(232), (8, 8, 8));
        assert_eq!(indexed_rgb(255), (238, 238, 238));
        // ANSI range hits the palette table.
        assert_eq!(indexed_rgb(1), ANSI16[1]);
    }

    #[test]
    fn terminal_bg_is_090909() {
        let bg = terminal_bg();
        assert_eq!(bg.s, 0.0);
        assert!((bg.l - 9.0 / 255.0).abs() < 1e-4);
    }

    #[test]
    fn timing_constants_match_spec() {
        assert_eq!(COALESCE_MS, 12);
        assert_eq!(RESIZE_DEBOUNCE_MS, 80);
    }
}
