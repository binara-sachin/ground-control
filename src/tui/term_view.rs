//! Output pane (§10.4): output captured from a tmux pane is fed through a `vt100` parser and
//! the parsed screen is drawn, so colours, `\r` progress lines and
//! box-drawing tables render as in a terminal. 120 columns wide; the pane
//! scrolls horizontally when narrower.

use std::num::NonZeroU16;
use std::sync::LazyLock;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use regex::Regex;

use super::theme;

const COLUMNS: u16 = 120;
const SCROLLBACK: usize = 10_000;
const MAX_RAW: usize = 8 * 1024 * 1024;

pub struct TermView {
    parser: vt100::Parser,
    raw: Vec<u8>,
    rows: u16,
    /// Stick to the newest output.
    pub follow: bool,
    /// Lines scrolled back from the bottom when not following.
    scroll: usize,
    pub hscroll: u16,
    /// A read is in flight.
    pub loading: bool,
    /// The source will produce no more output.
    pub complete: bool,
}

impl Default for TermView {
    fn default() -> Self {
        Self::new()
    }
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

impl TermView {
    pub fn new() -> Self {
        TermView {
            parser: vt100::Parser::new(nz(24), nz(COLUMNS), SCROLLBACK),
            raw: Vec::new(),
            rows: 24,
            follow: true,
            scroll: 0,
            hscroll: 0,
            loading: false,
            complete: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Feeds output. Pipes carry bare `\n`; a terminal's tty layer would turn
    /// it into `\r\n`, so that is done here.
    pub fn feed(&mut self, data: &str) {
        let bytes = data.replace('\n', "\r\n").into_bytes();
        self.parser.process(&bytes);
        self.raw.extend_from_slice(&bytes);
        if self.raw.len() > MAX_RAW {
            let cut = self.raw.len() - MAX_RAW / 2;
            self.raw.drain(..cut);
        }
    }

    /// Replaces the content with `data`, keeping the scroll position and
    /// follow mode. Output is re-read whole from the tmux pane, so this is how
    /// views stay current.
    pub fn replace(&mut self, data: &str) {
        let (follow, scroll, hscroll, rows) = (self.follow, self.scroll, self.hscroll, self.rows);
        *self = TermView::new();
        self.feed(data);
        self.resize(rows);
        (self.follow, self.scroll, self.hscroll) = (follow, scroll, hscroll);
    }

    pub fn reset(&mut self) {
        *self = TermView { follow: self.follow, ..TermView::new() };
    }

    fn resize(&mut self, rows: u16) {
        if rows == self.rows || rows == 0 {
            return;
        }
        self.rows = rows;
        self.parser = vt100::Parser::new(nz(rows), nz(COLUMNS), SCROLLBACK);
        self.parser.process(&self.raw);
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.follow = false;
        self.scroll = self.scroll.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        if self.scroll == 0 {
            self.follow = true;
        }
    }

    pub fn scroll_to_end(&mut self) {
        self.scroll = 0;
        self.follow = true;
    }

    pub fn scroll_left(&mut self, n: u16) {
        self.hscroll = self.hscroll.saturating_sub(n);
    }

    pub fn scroll_right(&mut self, n: u16, view_width: u16) {
        self.hscroll = (self.hscroll + n).min(COLUMNS.saturating_sub(view_width));
    }

    /// Text with colour codes stripped, for copying.
    pub fn plain_text(&self) -> String {
        strip_ansi(&String::from_utf8_lossy(&self.raw))
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.resize(area.height);
        if self.follow {
            self.scroll = 0;
        }
        self.parser.screen_mut().set_scrollback(self.scroll);
        self.scroll = self.parser.screen().scrollback();
        self.hscroll = self.hscroll.min(COLUMNS.saturating_sub(area.width));
        let screen = self.parser.screen();
        for row in 0..area.height {
            let y = area.y + row;
            let mut x = area.x;
            let mut col = self.hscroll;
            while x < area.x + area.width && col < COLUMNS {
                let Some(c) = screen.cell(row, col) else { break };
                if c.is_wide_continuation() {
                    col += 1;
                    continue;
                }
                let w: u16 = if c.is_wide() { 2 } else { 1 };
                let contents = c.contents();
                let style = cell_style(c);
                if x + w > area.x + area.width {
                    buf[(x, y)].set_symbol(" ").set_style(style);
                    break;
                }
                let sym = if contents.is_empty() { " " } else { contents };
                buf[(x, y)].set_symbol(sym).set_style(style);
                if w == 2 {
                    buf[(x + 1, y)].set_symbol("");
                }
                x += w;
                col += w;
            }
        }
    }

    /// Lines scrolled back, for the pane title.
    pub fn scrolled(&self) -> usize {
        self.scroll
    }
}

fn map_color(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => match i {
            0 => Color::Rgb(0x3b, 0x41, 0x50),
            1 => theme::DANGER,
            2 => theme::SUCCESS,
            3 => theme::WARNING,
            4 => theme::INFO,
            5 => Color::Rgb(0xc7, 0x92, 0xea),
            6 => Color::Rgb(0x89, 0xdd, 0xff),
            7 => theme::TEXT,
            8 => theme::MUTED,
            9 => Color::Rgb(0xff, 0x8b, 0x92),
            10 => Color::Rgb(0xc3, 0xe8, 0x8d),
            11 => Color::Rgb(0xff, 0xcb, 0x6b),
            12 => Color::Rgb(0x9c, 0xc4, 0xff),
            13 => Color::Rgb(0xdd, 0xa8, 0xff),
            14 => Color::Rgb(0xa3, 0xf7, 0xff),
            15 => Color::White,
            n => Color::Indexed(n),
        },
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn cell_style(c: &vt100::Cell) -> Style {
    let mut fg = map_color(c.fgcolor(), theme::TEXT);
    let mut bg = map_color(c.bgcolor(), theme::BG);
    if c.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut s = Style::new().fg(fg).bg(bg);
    if c.bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    if c.dim() {
        s = s.add_modifier(Modifier::DIM);
    }
    if c.italic() {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if c.underline() {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b(\[[0-9;?<=>]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|[@-Z\\-_])").unwrap());

/// Removes escape sequences and resolves `\r` overwrites line by line.
fn strip_ansi(s: &str) -> String {
    let clean = ANSI.replace_all(s, "");
    let mut out = String::with_capacity(clean.len());
    for (i, line) in clean.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str(line.rsplit('\r').next().unwrap_or(line));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colour_and_progress() {
        let s = "\x1b[32m✔ ok\x1b[0m\r\nprog 10%\rprog 100%\n\x1b]0;title\x07done";
        assert_eq!(strip_ansi(s), "✔ ok\nprog 100%\ndone");
    }

    #[test]
    fn renders_progress_lines_and_wide_chars() {
        let mut v = TermView::new();
        v.feed("⛅ wrangler\nstep 1/3\rstep 3/3\n");
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        let line0: String = (0..20).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        let line1: String = (0..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert!(line0.starts_with("⛅"), "{line0:?}");
        assert!(line0.contains("wrangler"));
        assert!(line1.starts_with("step 3/3"), "{line1:?}");
    }

    #[test]
    fn scrolls_back() {
        let mut v = TermView::new();
        for i in 0..50 {
            v.feed(&format!("line {i}\n"));
        }
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        let first: String = (0..10).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(first.starts_with("line 46"), "{first:?}");
        v.scroll_up(10);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        let first: String = (0..10).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(first.starts_with("line 36"), "{first:?}");
        assert!(!v.follow);
    }
}
