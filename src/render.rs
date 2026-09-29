//! Screen buffer for rendering.
//!
//! The app renders into a grid of [Cell]s ([RBuf]). Every component draws
//! into a sub-rect via:
//!
//! ```ignore
//! fn draw(&self, buf: &mut RBuf, x: usize, y: usize, w: usize, h: usize, focused: bool);
//! ```
//!
//! Components must fill their whole rect (with their own background) so that
//! overlays completely cover whatever was drawn before. The final screen is
//! blitted to the ratatui `Buffer` once per frame.
//!
//! Border helpers port the semantics of superfile's lipgloss-based
//! `rendering.Renderer` + `BorderConfig` (title in the top border, info items
//! in the bottom border, section divider rows with middle border corners).

use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthChar;

use crate::util::str_width;

/// Border glyph set (this ratatui build has no `BorderSet` type).
///
/// Each field is a (single-char) border glyph from the user's config.
#[derive(Clone, Copy, Debug)]
pub struct BorderSet<'a> {
    pub top: &'a str,
    pub bottom: &'a str,
    pub left: &'a str,
    pub right: &'a str,
    pub top_left: &'a str,
    pub top_right: &'a str,
    pub bottom_left: &'a str,
    pub bottom_right: &'a str,
    pub middle_left: &'a str,
    pub middle_right: &'a str,
}

impl<'a> BorderSet<'a> {
    /// Standard single-line box-drawing glyphs.
    pub fn plain() -> Self {
        Self {
            top: "─",
            bottom: "─",
            left: "│",
            right: "│",
            top_left: "╭",
            top_right: "╮",
            bottom_left: "╰",
            bottom_right: "╯",
            middle_left: "├",
            middle_right: "┤",
        }
    }

    /// First char of a glyph (border glyphs are single-char strings).
    pub fn ch(s: &str) -> char {
        s.chars().next().unwrap_or(' ')
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attr: Modifier,
}

/// Partial style: `None` fields are inherited from the existing cell.
#[derive(Clone, Copy, Default)]
pub struct St {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub attr: Modifier,
}

impl St {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn fg(mut self, c: Color) -> Self {
        self.fg = Some(c);
        self
    }
    pub fn bg(mut self, c: Color) -> Self {
        self.bg = Some(c);
        self
    }
    pub fn fg_bg(fg: Color, bg: Color) -> Self {
        St {
            fg: Some(fg),
            bg: Some(bg),
            attr: Modifier::empty(),
        }
    }
    pub fn bold(self) -> Self {
        self.attr(Modifier::BOLD)
    }
    pub fn italic(self) -> Self {
        self.attr(Modifier::ITALIC)
    }
    pub fn underline(self) -> Self {
        self.attr(Modifier::UNDERLINED)
    }
    pub fn dim(self) -> Self {
        self.attr(Modifier::DIM)
    }
    fn attr(mut self, m: Modifier) -> Self {
        self.attr |= m;
        self
    }
}

pub struct RBuf {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
}

impl RBuf {
    /// A fresh buffer filled with `bg`.
    pub fn new(w: usize, h: usize, bg: Color) -> Self {
        let cell = Cell {
            ch: ' ',
            fg: Color::Reset,
            bg,
            attr: Modifier::empty(),
        };
        Self {
            w,
            h,
            cells: vec![cell; w * h],
        }
    }

    pub fn w(&self) -> usize {
        self.w
    }
    pub fn h(&self) -> usize {
        self.h
    }

    pub fn cell(&self, x: usize, y: usize) -> Option<&Cell> {
        (x < self.w && y < self.h).then(|| &self.cells[y * self.w + x])
    }

    pub fn cell_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        (x < self.w && y < self.h).then(|| &mut self.cells[y * self.w + x])
    }

    /// Fill a rect, clearing chars to spaces and (re)applying the style.
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, st: St) {
        for yy in y..y + h {
            for xx in x..x + w {
                if let Some(c) = self.cell_mut(xx, yy) {
                    c.ch = ' ';
                    if let Some(fg) = st.fg {
                        c.fg = fg;
                    }
                    if let Some(bg) = st.bg {
                        c.bg = bg;
                    }
                    c.attr = st.attr;
                }
            }
        }
    }

    /// Draw a single char (replaces the cell's char, merges style).
    pub fn put_char(&mut self, x: usize, y: usize, ch: char, st: St) {
        if let Some(c) = self.cell_mut(x, y) {
            c.ch = ch;
            if let Some(fg) = st.fg {
                c.fg = fg;
            }
            if let Some(bg) = st.bg {
                c.bg = bg;
            }
            c.attr |= st.attr;
        }
    }

    /// Draw a string left-to-right from (x, y), clipping at the right/bottom
    /// bounds. Wide chars take two cells, zero-width chars are skipped.
    /// Returns the x position after the last drawn char.
    pub fn put_str(&mut self, mut x: usize, y: usize, s: &str, st: St) -> usize {
        for ch in s.chars() {
            let cw = ch.width().unwrap_or(1);
            if cw == 0 {
                continue;
            }
            if y >= self.h || x + cw > self.w {
                break;
            }
            for i in 0..cw {
                if let Some(c) = self.cell_mut(x + i, y) {
                    c.ch = ch;
                    if let Some(fg) = st.fg {
                        c.fg = fg;
                    }
                    if let Some(bg) = st.bg {
                        c.bg = bg;
                    }
                    c.attr |= st.attr;
                }
            }
            x += cw;
        }
        x
    }

    /// Draw a line into a fixed-width slot: the whole slot gets the style's
    /// background, then the string is drawn (clipped to the slot).
    pub fn put_line(&mut self, x: usize, y: usize, w: usize, s: &str, st: St) {
        if let Some(bg) = st.bg {
            for xx in x..(x + w).min(self.w) {
                if let Some(c) = self.cell_mut(xx, y) {
                    c.bg = bg;
                }
            }
        }
        self.put_str(x, y, s, st);
    }

    fn hline(&mut self, x: usize, y: usize, w: usize, ch: char, st: St) {
        for i in 0..w {
            self.put_char(x + i, y, ch, st);
        }
    }

    fn vline(&mut self, x: usize, y: usize, h: usize, ch: char, st: St) {
        for i in 0..h {
            self.put_char(x, y + i, ch, st);
        }
    }

    /// Plain border frame around (x, y, w, h).
    pub fn border(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        set: BorderSet<'_>,
        fg: Color,
        bg: Color,
    ) {
        if w < 2 || h < 2 {
            return;
        }
        let st = St::fg_bg(fg, bg);
        self.put_char(x, y, BorderSet::ch(set.top_left), st);
        self.put_char(x + w - 1, y, BorderSet::ch(set.top_right), st);
        self.put_char(x, y + h - 1, BorderSet::ch(set.bottom_left), st);
        self.put_char(x + w - 1, y + h - 1, BorderSet::ch(set.bottom_right), st);
        self.hline(x + 1, y, w - 2, BorderSet::ch(set.top), st);
        self.hline(x + 1, y + h - 1, w - 2, BorderSet::ch(set.bottom), st);
        self.vline(x, y + 1, h - 2, BorderSet::ch(set.left), st);
        self.vline(x + w - 1, y + 1, h - 2, BorderSet::ch(set.right), st);
    }

    /// Border with a title embedded in the top border, e.g.
    /// `╭─┤ Metadata ├────────────╮`. Ported from Go's
    /// `BorderConfig.GetBorder` (left margin of 1 top char when it fits).
    pub fn border_title(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        set: BorderSet<'_>,
        fg: Color,
        bg: Color,
        title: &str,
        title_fg: Color,
    ) {
        self.border(x, y, w, h, set, fg, bg);
        let actual = w.saturating_sub(2);
        if title.is_empty() || actual < 5 {
            return;
        }
        let avail = actual - 4;
        let t = crate::util::plain_truncate(title, avail);
        let rem = actual - 4 - str_width(&t);
        let margin = if rem > 1 { 1 } else { 0 };
        let st = St::fg_bg(fg, bg);
        let tst = St::fg_bg(title_fg, bg);
        let mut cx = x + 1 + margin;
        self.put_char(cx, y, BorderSet::ch(set.middle_right), st);
        cx += 1;
        self.put_char(cx, y, ' ', tst);
        cx += 1;
        cx = self.put_str(cx, y, &t, tst);
        self.put_char(cx, y, ' ', tst);
        cx += 1;
        self.put_char(cx, y, BorderSet::ch(set.middle_left), st);
    }

    /// Border with info items embedded in the bottom border, right aligned.
    /// Each item is rendered as `MiddleRight item MiddleLeft Bottom`; the
    /// remaining width is filled with bottom border chars. Ported from Go's
    /// `BorderConfig.GetBorder` (items are plain-truncated to
    /// `(actualWidth / cnt) - 1`; skipped unless `actualWidth >= 4 * cnt`).
    pub fn border_info(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        set: BorderSet<'_>,
        fg: Color,
        bg: Color,
        items: &[&str],
    ) {
        self.border(x, y, w, h, set, fg, bg);
        let cnt = items.len();
        let actual = w.saturating_sub(2);
        if cnt == 0 || h < 2 || actual < cnt * 4 {
            return;
        }
        let st = St::fg_bg(fg, bg);
        let mrw = str_width(set.middle_right).max(1);
        let mlw = str_width(set.middle_left).max(1);
        let bw = str_width(set.bottom).max(1);
        let avail = (actual / cnt).saturating_sub(mrw + mlw + bw);
        let mut text = String::new();
        for it in items {
            text.push_str(set.middle_right);
            text.push_str(&crate::util::plain_truncate(it, avail));
            text.push_str(set.middle_left);
            text.push_str(set.bottom);
        }
        let rem = actual.saturating_sub(str_width(&text));
        let mut bottom = String::with_capacity(actual);
        for _ in 0..rem {
            bottom.push_str(set.bottom);
        }
        bottom.push_str(&text);
        self.put_str(x + 1, y + h - 1, &bottom, st);
    }

    /// A section divider row at (x, y) spanning w cols: middle-left / middle-right
    /// border chars at the ends, top border chars in between.
    pub fn section_divider(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        set: BorderSet<'_>,
        fg: Color,
        bg: Color,
    ) {
        if w < 2 {
            return;
        }
        let st = St::fg_bg(fg, bg);
        self.put_char(x, y, BorderSet::ch(set.middle_left), st);
        self.hline(x + 1, y, w - 2, BorderSet::ch(set.top), st);
        self.put_char(x + w - 1, y, BorderSet::ch(set.middle_right), st);
    }

    /// A bottom border row with middle dividers at both ends
    /// (Go: BottomMiddleBorderSplit).
    pub fn bottom_split(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        set: BorderSet<'_>,
        fg: Color,
        bg: Color,
    ) {
        if w < 2 {
            return;
        }
        let st = St::fg_bg(fg, bg);
        self.put_char(x, y, BorderSet::ch(set.middle_left), st);
        self.hline(x + 1, y, w - 2, BorderSet::ch(set.bottom), st);
        self.put_char(x + w - 1, y, BorderSet::ch(set.middle_right), st);
    }

    /// Copy this buffer onto a ratatui buffer at (0, 0), clipping to the
    /// target's area.
    pub fn blit(&self, target: &mut Buffer) {
        let area = target.area();
        let w = self.w.min(area.width as usize);
        let h = self.h.min(area.height as usize);
        for y in 0..h {
            for x in 0..w {
                let c = &self.cells[y * self.w + x];
                if let Some(tc) = target.cell_mut(Position { x: x as u16, y: y as u16 }) {
                    tc.set_char(c.ch).set_fg(c.fg).set_bg(c.bg);
                    tc.modifier = c.attr;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> BorderSet<'static> {
        BorderSet::plain()
    }

    #[test]
    fn put_str_clips() {
        let mut b = RBuf::new(5, 1, Color::Black);
        let x = b.put_str(0, 0, "hello", St::fg_bg(Color::White, Color::Black));
        assert_eq!(x, 5);
        assert_eq!(b.cell(4, 0).unwrap().ch, 'o');
        // clipping is per-character: 'x' fits in the last column, 'y' clips
        b.put_str(4, 0, "xy", St::new());
        assert_eq!(b.cell(4, 0).unwrap().ch, 'x');
        // fully out of bounds: nothing can be written
        let before = b.cell(4, 0).unwrap().ch;
        b.put_str(5, 0, "zz", St::new());
        assert_eq!(b.cell(4, 0).unwrap().ch, before);
    }

    #[test]
    fn border_title_layout() {
        let mut b = RBuf::new(20, 5, Color::Black);
        b.border_title(0, 0, 20, 5, set(), Color::White, Color::Black, "Processes", Color::Yellow);
        // top row: ╭ ─ ┤ space P...s space ├ ──...
        let row: String = (0..20).map(|x| b.cell(x, 0).unwrap().ch).collect();
        assert_eq!(row.as_bytes()[0], "╭".as_bytes()[0]);
        assert!(row.contains("┤ Processes ├"));
        assert_eq!(row.chars().count(), 20);
    }

    #[test]
    fn border_info_layout() {
        let mut b = RBuf::new(30, 3, Color::Black);
        b.border_info(0, 0, 30, 3, set(), Color::White, Color::Black, &["abc", "def"]);
        let row: String = (0..30).map(|x| b.cell(x, 2).unwrap().ch).collect();
        assert!(row.contains("┤abc├─┤def├"));
    }

    #[test]
    fn fill_clears() {
        let mut b = RBuf::new(4, 2, Color::Black);
        b.put_str(0, 0, "abcd", St::new().fg(Color::White));
        b.fill(0, 0, 4, 2, St::new().bg(Color::Red));
        assert_eq!(b.cell(0, 0).unwrap().ch, ' ');
        assert_eq!(b.cell(0, 0).unwrap().bg, Color::Red);
    }
}
