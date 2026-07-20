//! The terminal emulator core: `alacritty_terminal`'s `Term` + vte's ANSI
//! `Processor` wrapped as a pure state machine.
//!
//! Bytes in ([`Emulator::feed`] — the decoded `SubscribeTerminal` Data frames),
//! grid snapshots out ([`Emulator::line`], [`Emulator::cursor`]). No I/O, no
//! timers, no gpui: the panel owns RPC and scheduling, the view owns paint.
//! That split makes the whole escape-sequence surface unit-testable with
//! scripted byte strings.
//!
//! API notes for the pinned `alacritty_terminal 0.26` / `vte 0.15`:
//! - `Processor::advance` consumes a byte slice; `Term` implements the
//!   `vte::ansi::Handler` trait directly, so no event-loop machinery is needed.
//! - `Term::new` takes any `grid::Dimensions` impl — [`GridSize`] here (the
//!   crate's own `TermSize` lives in a `term::test` helper module).
//! - Query responses (DSR/DA/…) surface as `Event::PtyWrite` on the listener;
//!   [`Emulator::feed`] returns them so the panel can write them back.

use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb as AnsiRgb,
};

/// Scrollback history kept client-side (lines). The engine's replay window is
/// bounded separately (1 MiB); this only caps what stays scrollable in the UI.
pub const SCROLLBACK_LINES: usize = 10_000;

/// Viewport dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(2),
            rows: rows.max(1),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// A cell's paint color, decoupled from the palette: the view resolves these
/// against the theme (default fg/bg, 256-color index, or direct RGB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// Indexed color: 0-15 ANSI, 16-231 color cube, 232-255 grayscale ramp.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

fn map_color(color: AnsiColor) -> CellColor {
    match color {
        AnsiColor::Spec(AnsiRgb { r, g, b }) => CellColor::Rgb(r, g, b),
        AnsiColor::Indexed(ix) => CellColor::Indexed(ix),
        AnsiColor::Named(named) => {
            let ix = named as usize;
            if ix < 16 {
                return CellColor::Indexed(ix as u8);
            }
            match named {
                NamedColor::Background => CellColor::Background,
                // Dim named colors fold onto their base index; the DIM flag
                // still travels on the cell for paint-time dimming.
                NamedColor::DimBlack
                | NamedColor::DimRed
                | NamedColor::DimGreen
                | NamedColor::DimYellow
                | NamedColor::DimBlue
                | NamedColor::DimMagenta
                | NamedColor::DimCyan
                | NamedColor::DimWhite => {
                    CellColor::Indexed((ix - NamedColor::DimBlack as usize) as u8)
                }
                _ => CellColor::Foreground,
            }
        }
    }
}

/// One rendered cell: char + colors + the flags paint cares about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSnapshot {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// A double-width char (occupies this cell plus the next spacer cell).
    pub wide: bool,
    /// The spacer half of a wide char — never shaped, only background-painted.
    pub wide_spacer: bool,
}

impl CellSnapshot {
    /// Effective paint colors after INVERSE/HIDDEN resolution.
    pub fn display_colors(&self) -> (CellColor, CellColor) {
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        if self.hidden { (bg, bg) } else { (fg, bg) }
    }
}

/// Cursor position in viewport coordinates (row 0 = top of the visible grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
}

/// Captures `Term` callbacks. Interior-mutable because `EventListener::send_event`
/// takes `&self`; single-threaded (the emulator lives inside a gpui entity).
#[derive(Default, Clone)]
struct EventCapture {
    events: Rc<RefCell<Vec<Event>>>,
}

impl EventListener for EventCapture {
    fn send_event(&self, event: Event) {
        self.events.borrow_mut().push(event);
    }
}

/// The emulator: a pure fold of PTY bytes into a renderable grid.
pub struct Emulator {
    term: Term<EventCapture>,
    parser: Processor,
    capture: EventCapture,
    title: Option<String>,
    bell: bool,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        let capture = EventCapture::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let term = Term::new(config, &GridSize::new(cols, rows), capture.clone());
        Self {
            term,
            parser: Processor::new(),
            capture,
            title: None,
            bell: false,
        }
    }

    /// Advance the state machine over decoded PTY output. Returns bytes the
    /// terminal wants written back to the PTY (DSR/DA query responses etc.).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        let mut responses = Vec::new();
        for event in self.capture.events.borrow_mut().drain(..) {
            match event {
                Event::PtyWrite(text) => responses.extend_from_slice(text.as_bytes()),
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                Event::Bell => self.bell = true,
                _ => {}
            }
        }
        responses
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(GridSize::new(cols, rows));
    }

    pub fn cols(&self) -> usize {
        self.term.columns()
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    /// OSC title, if the running program set one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// True once a BEL arrived; reading clears it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Arrow keys should send SS3 (`ESC O A`) instead of CSI.
    pub fn app_cursor_mode(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Pastes should be wrapped in `ESC [200~` / `ESC [201~`.
    pub fn bracketed_paste_mode(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Lines scrolled back into history (0 = pinned to the live bottom).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines available above the viewport.
    pub fn history_lines(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Scroll the view: positive = up into history, negative = toward live.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Snapshot one viewport row (0 = top) honoring the scrollback offset.
    pub fn line(&self, viewport_row: usize) -> Vec<CellSnapshot> {
        let offset = self.display_offset() as i32;
        let line = Line(viewport_row as i32 - offset);
        let grid = self.term.grid();
        let row = &grid[line];
        (0..self.cols())
            .map(|col| {
                let cell = &row[Column(col)];
                CellSnapshot {
                    ch: cell.c,
                    fg: map_color(cell.fg),
                    bg: map_color(cell.bg),
                    bold: cell.flags.intersects(Flags::BOLD),
                    dim: cell.flags.intersects(Flags::DIM),
                    italic: cell.flags.intersects(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: cell.flags.intersects(Flags::INVERSE),
                    hidden: cell.flags.intersects(Flags::HIDDEN),
                    wide: cell.flags.intersects(Flags::WIDE_CHAR),
                    wide_spacer: cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                }
            })
            .collect()
    }

    /// All viewport rows, top to bottom.
    pub fn lines(&self) -> Vec<Vec<CellSnapshot>> {
        (0..self.rows()).map(|r| self.line(r)).collect()
    }

    /// Cursor in viewport coordinates; `None` when hidden or scrolled out.
    pub fn cursor(&self) -> Option<CursorSnapshot> {
        let content = self.term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }
        let Point { line, column } = content.cursor.point;
        let row = line.0 + self.display_offset() as i32;
        if row < 0 || row >= self.rows() as i32 {
            return None;
        }
        Some(CursorSnapshot {
            row: row as usize,
            col: column.0,
        })
    }

    /// Test/diagnostic helper: a viewport row as trimmed text (wide-char
    /// spacers skipped).
    pub fn row_text(&self, viewport_row: usize) -> String {
        let mut text: String = self
            .line(viewport_row)
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.ch)
            .collect();
        while text.ends_with(' ') {
            text.pop();
        }
        text
    }
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("cols", &self.cols())
            .field("rows", &self.rows())
            .field("display_offset", &self.display_offset())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu(cols: u16, rows: u16) -> Emulator {
        Emulator::new(cols, rows)
    }

    #[test]
    fn plain_text_lands_on_row_zero() {
        let mut e = emu(20, 5);
        e.feed(b"hello");
        assert_eq!(e.row_text(0), "hello");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 5 }));
    }

    #[test]
    fn crlf_moves_lines_and_cr_returns_to_column_zero() {
        let mut e = emu(20, 5);
        e.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(e.row_text(0), "one");
        assert_eq!(e.row_text(1), "two");
        assert_eq!(e.row_text(2), "three");
        e.feed(b"\rXX");
        assert_eq!(e.row_text(2), "XXree");
    }

    #[test]
    fn long_line_wraps_at_the_grid_width() {
        let mut e = emu(10, 4);
        e.feed(b"abcdefghijKLM");
        assert_eq!(e.row_text(0), "abcdefghij");
        assert_eq!(e.row_text(1), "KLM");
    }

    #[test]
    fn sgr_colors_and_attributes() {
        let mut e = emu(40, 4);
        e.feed(b"\x1b[31mred\x1b[0m plain \x1b[1;44mboldbg\x1b[0m");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(1));
        assert_eq!(line[0].bg, CellColor::Background);
        // After reset: defaults.
        assert_eq!(line[4].fg, CellColor::Foreground);
        // Bold + blue background segment starts at col 10 ("red plain " = 10).
        let bold_cell = line[10];
        assert!(bold_cell.bold);
        assert_eq!(bold_cell.bg, CellColor::Indexed(4));
    }

    #[test]
    fn bright_256_and_truecolor_sgr() {
        let mut e = emu(40, 2);
        e.feed(b"\x1b[95mA\x1b[38;5;196mB\x1b[38;2;10;20;30mC");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(13)); // bright magenta
        assert_eq!(line[1].fg, CellColor::Indexed(196));
        assert_eq!(line[2].fg, CellColor::Rgb(10, 20, 30));
    }

    #[test]
    fn inverse_and_hidden_resolve_in_display_colors() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[7mI\x1b[0m\x1b[8mH");
        let inv = e.line(0)[0];
        assert!(inv.inverse);
        assert_eq!(
            inv.display_colors(),
            (CellColor::Background, CellColor::Foreground)
        );
        let hid = e.line(0)[1];
        assert!(hid.hidden);
        let (fg, bg) = hid.display_colors();
        assert_eq!(fg, bg, "hidden text paints foreground as background");
    }

    #[test]
    fn cursor_addressing_and_relative_moves() {
        let mut e = emu(20, 6);
        e.feed(b"\x1b[3;5Hx");
        // CSI H is 1-based; cell written at row 2, col 4; cursor advanced by 1.
        assert_eq!(e.line(2)[4].ch, 'x');
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 5 }));
        e.feed(b"\x1b[2D"); // left twice
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 3 }));
        e.feed(b"\x1b[A"); // up
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 1, col: 3 }));
    }

    #[test]
    fn clear_screen_and_home() {
        let mut e = emu(20, 4);
        e.feed(b"aaa\r\nbbb\r\nccc");
        e.feed(b"\x1b[2J\x1b[H");
        for row in 0..4 {
            assert_eq!(e.row_text(row), "");
        }
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 0 }));
        e.feed(b"fresh");
        assert_eq!(e.row_text(0), "fresh");
    }

    #[test]
    fn erase_line_variants() {
        let mut e = emu(20, 2);
        e.feed(b"abcdef\x1b[3D\x1b[K"); // erase from cursor (col 3) to end
        assert_eq!(e.row_text(0), "abc");
    }

    #[test]
    fn scrollback_history_and_scrolling() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        // Viewport shows the tail (line7, line8, then the blank prompt row).
        assert_eq!(e.row_text(0), "line7");
        assert_eq!(e.history_lines(), 6);
        assert_eq!(e.display_offset(), 0);
        // Scroll up into history.
        e.scroll(2);
        assert_eq!(e.display_offset(), 2);
        assert_eq!(e.row_text(0), "line5");
        // Cursor is below the viewport while scrolled back.
        assert_eq!(e.cursor(), None);
        // Over-scroll clamps to the top of history.
        e.scroll(100);
        assert_eq!(e.display_offset(), 6);
        assert_eq!(e.row_text(0), "line1");
        e.scroll_to_bottom();
        assert_eq!(e.display_offset(), 0);
        assert_eq!(e.row_text(0), "line7");
    }

    #[test]
    fn alt_screen_restores_primary_content() {
        let mut e = emu(20, 4);
        e.feed(b"primary");
        // Enter the alt screen; 1049 keeps the cursor position, so home first.
        e.feed(b"\x1b[?1049h\x1b[H");
        e.feed(b"alt-content");
        assert_eq!(e.row_text(0), "alt-content");
        e.feed(b"\x1b[?1049l"); // leave
        assert_eq!(e.row_text(0), "primary");
    }

    #[test]
    fn dsr_cursor_report_produces_pty_response() {
        let mut e = emu(20, 4);
        e.feed(b"\x1b[2;3H");
        let responses = e.feed(b"\x1b[6n");
        assert_eq!(String::from_utf8_lossy(&responses), "\x1b[2;3R");
    }

    #[test]
    fn osc_title_and_bell() {
        let mut e = emu(20, 2);
        assert_eq!(e.title(), None);
        e.feed(b"\x1b]0;my title\x07");
        assert_eq!(e.title(), Some("my title"));
        assert!(!e.take_bell());
        e.feed(b"\x07");
        assert!(e.take_bell());
        assert!(!e.take_bell(), "bell reads clear it");
    }

    #[test]
    fn app_cursor_and_bracketed_paste_modes_toggle() {
        let mut e = emu(10, 2);
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?1h");
        assert!(e.app_cursor_mode());
        e.feed(b"\x1b[?1l");
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?2004h");
        assert!(e.bracketed_paste_mode());
    }

    #[test]
    fn hidden_cursor_mode() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[?25l");
        assert_eq!(e.cursor(), None);
        e.feed(b"\x1b[?25h");
        assert!(e.cursor().is_some());
    }

    #[test]
    fn resize_preserves_content_and_reflows_cursor() {
        let mut e = emu(20, 5);
        e.feed(b"keepme\r\nsecond");
        e.resize(30, 3);
        assert_eq!(e.cols(), 30);
        assert_eq!(e.rows(), 3);
        assert_eq!(e.row_text(0), "keepme");
        assert_eq!(e.row_text(1), "second");
    }

    #[test]
    fn wide_chars_occupy_two_cells_with_spacer() {
        let mut e = emu(10, 2);
        e.feed("宽w".as_bytes());
        let line = e.line(0);
        assert!(line[0].wide);
        assert_eq!(line[0].ch, '宽');
        assert!(line[1].wide_spacer);
        assert_eq!(line[2].ch, 'w');
        assert_eq!(e.row_text(0), "宽w");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 3 }));
    }

    #[test]
    fn utf8_split_across_feeds_reassembles() {
        let mut e = emu(10, 2);
        let bytes = "é".as_bytes();
        e.feed(&bytes[..1]);
        e.feed(&bytes[1..]);
        assert_eq!(e.row_text(0), "é");
    }
}
