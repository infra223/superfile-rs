//! A minimal text input widget (buffer + byte-offset cursor + horizontal
//! scroll), the Rust stand-in for bubbles' `textinput`.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::keys::{Key, KeyKind};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    Edited,
    CursorMoved,
    BackspaceAtStart,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct TextInput {
    pub buf: String,
    /// byte offset of the cursor
    pub cursor: usize,
    /// leftmost visible byte offset (for horizontal scrolling)
    pub scroll: usize,
    /// render width in columns (0 = unbounded)
    pub width: usize,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            scroll: 0,
            width: 0,
        }
    }

    pub fn with_value(v: String) -> Self {
        let len = v.len();
        Self {
            buf: v,
            cursor: len,
            scroll: 0,
            width: 0,
        }
    }

    pub fn value(&self) -> &str {
        &self.buf
    }

    pub fn set_width(&mut self, w: usize) {
        self.width = w;
    }

    fn char_len_to_byte(&self, target: usize) -> usize {
        let mut i = 0;
        for c in self.buf.chars() {
            if i == target {
                break;
            }
            i += c.len_utf8();
        }
        i.min(self.buf.len())
    }

    fn byte_to_char_len(&self, byte: usize) -> usize {
        self.buf[..byte.min(self.buf.len())].chars().count()
    }

    fn col_width(&self, from: usize, to: usize) -> usize {
        self.buf[from..to].width()
    }

    fn fixup_cursor(&mut self) {
        if self.cursor > self.buf.len() {
            self.cursor = self.buf.len();
        }
        // keep cursor visible
        let cw = self.width;
        if cw == 0 {
            return;
        }
        let col = self.col_width(self.scroll, self.cursor);
        if col > cw {
            // scroll right: find the smallest scroll offset where the cursor is visible
            let mut s = self.scroll;
            let mut w = 0;
            for c in self.buf[s..].chars() {
                let cl = c.len_utf8();
                if w + c.width().unwrap_or(1) > cw {
                    break;
                }
                w += c.width().unwrap_or(1);
                s += cl;
                if s >= self.cursor {
                    break;
                }
            }
            self.scroll = s.min(self.cursor);
        } else if self.col_width(self.scroll, self.cursor) + self.col_width(self.cursor, self.buf.len()) <= cw
        {
            // can we scroll back left?
            let mut s = self.cursor;
            let mut w = 0;
            for c in self.buf[..self.cursor].chars().rev() {
                let cw2 = c.width().unwrap_or(1);
                if w + cw2 > cw {
                    break;
                }
                w += cw2;
                s = s - c.len_utf8();
            }
            if s < self.scroll {
                self.scroll = s;
            }
        }
    }

    /// Feed a normalized key into the buffer. Returns what happened.
    pub fn handle_key(&mut self, k: &Key) -> InputEvent {
        match k.kind {
            KeyKind::Ch(c) => {
                if c == '\0' || k.ctrl || k.alt {
                    return InputEvent::Ignored;
                }
                let ch = k.raw_char.unwrap_or(c);
                let ch = if ch == '\t' { ' ' } else { ch };
                let b = ch.len_utf8();
                self.buf.insert_str(self.cursor, &ch.to_string());
                self.cursor += b;
                self.fixup_cursor();
                InputEvent::Edited
            }
            KeyKind::Backspace => {
                if self.cursor == 0 {
                    return InputEvent::BackspaceAtStart;
                }
                let start = self.buf[..self.cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.buf.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.fixup_cursor();
                InputEvent::Edited
            }
            KeyKind::Delete => {
                if self.cursor < self.buf.len() {
                    let end =
                        self.cursor + self.buf[self.cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                    self.buf.replace_range(self.cursor..end, "");
                    self.fixup_cursor();
                    InputEvent::Edited
                } else {
                    InputEvent::Ignored
                }
            }
            KeyKind::Left => {
                if self.cursor > 0 {
                    let start = self.buf[..self.cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.cursor = start;
                    self.fixup_cursor();
                    InputEvent::CursorMoved
                } else {
                    InputEvent::Ignored
                }
            }
            KeyKind::Right => {
                if self.cursor < self.buf.len() {
                    let next =
                        self.cursor + self.buf[self.cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                    self.cursor = next;
                    self.fixup_cursor();
                    InputEvent::CursorMoved
                } else {
                    InputEvent::Ignored
                }
            }
            KeyKind::Home => {
                if self.cursor != 0 {
                    self.cursor = 0;
                    self.fixup_cursor();
                    InputEvent::CursorMoved
                } else {
                    InputEvent::Ignored
                }
            }
            KeyKind::End => {
                if self.cursor != self.buf.len() {
                    self.cursor = self.buf.len();
                    self.fixup_cursor();
                    InputEvent::CursorMoved
                } else {
                    InputEvent::Ignored
                }
            }
            _ => InputEvent::Ignored,
        }
    }

    /// Rendered line with a `│` cursor, clipped to the configured width.
    pub fn view(&self, focused: bool) -> String {
        let visible_end = if self.width == 0 {
            self.buf.len()
        } else {
            let mut s = self.scroll;
            let mut w = 0;
            for c in self.buf[self.scroll..].chars() {
                let cw = c.width().unwrap_or(1);
                if w + cw > self.width {
                    break;
                }
                w += cw;
                s += c.len_utf8();
            }
            s
        };
        let mut out = String::new();
        if focused {
            // chars before cursor
            let pre: String = self.buf[self.scroll..self.cursor].to_string();
            let post: String = self.buf[self.cursor..visible_end].to_string();
            out.push_str(&pre);
            out.push('│');
            out.push_str(&post);
        } else {
            out.push_str(&self.buf[self.scroll..visible_end]);
        }
        out
    }

    /// Character offset (not byte) of the cursor.
    pub fn cursor_char(&self) -> usize {
        self.byte_to_char_len(self.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys;
    use crate::keys::KeyKind;

    #[test]
    fn insert_and_backspace() {
        let mut ti = TextInput::new();
        ti.buf.insert_str(0, "abc");
        ti.cursor = 1;
        ti.buf.insert_str(ti.cursor, "X");
        assert_eq!(ti.buf, "aXbc");
        ti.cursor = 2; // a real insert moves the cursor past the new char
        ti.handle_key(&Key::new(KeyKind::Backspace));
        assert_eq!(ti.buf, "abc");
        assert_eq!(ti.cursor, 1);
    }

    #[test]
    fn ctrl_char_ignored() {
        let mut ti = TextInput::new();
        let k = Key {
            kind: KeyKind::Ch('c'),
            ctrl: true,
            shift: false,
            alt: false,
            raw_char: Some('c'),
        };
        assert_eq!(ti.handle_key(&k), InputEvent::Ignored);
        assert!(ti.buf.is_empty());
    }

    #[test]
    fn uppercase_inserted_verbatim() {
        let mut ti = TextInput::new();
        let k = Key {
            kind: KeyKind::Ch('q'),
            ctrl: false,
            shift: true,
            alt: false,
            raw_char: Some('Q'),
        };
        ti.handle_key(&k);
        assert_eq!(ti.buf, "Q");
        assert_eq!(keys::input_char(&k), Some('Q'));
    }
}
