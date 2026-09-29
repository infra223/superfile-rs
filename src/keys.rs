//! Hotkey parsing and matching.
//!
//! Hotkeys are configured as strings like `"ctrl+shift+a"`, `"Q"`, `"pgdown"`,
//! `"backspace"`, `"."`. A crossterm `KeyEvent` is normalized to the same
//! canonical form before comparison (chars are lower-cased; crossterm reports
//! shifted chars as uppercase with the SHIFT modifier, which normalizes to
//! lowercase + SHIFT, matching how `"Q"` is parsed). `raw_char` keeps the
//! original (case-sensitive) char for text input widgets.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyKind {
    Ch(char),
    Enter,
    Esc,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PgUp,
    PgDn,
    F(u8),
}

#[derive(Clone, Copy, Debug)]
pub struct Key {
    pub kind: KeyKind,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Original (non-normalized) char for text input; None for parsed hotkeys
    /// and non-char keys.
    pub raw_char: Option<char>,
}

/// Equality for hotkey matching. `raw_char` is deliberately excluded: parsed
/// hotkey strings carry `raw_char = None` while crossterm events carry the
/// actual character — including it would make e.g. "ctrl+c" never match a
/// real Ctrl+C press.
impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        self.kind == other.kind
            && self.ctrl == other.ctrl
            && self.shift == other.shift
            && self.alt == other.alt
    }
}
impl Eq for Key {}

impl Key {
    pub fn new(kind: KeyKind) -> Self {
        Key {
            kind,
            ctrl: false,
            shift: false,
            alt: false,
            raw_char: None,
        }
    }
}

/// Parse a hotkey string. Returns None for empty/invalid input.
pub fn parse(s: &str) -> Option<Key> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut k = Key {
        kind: KeyKind::Ch('\0'),
        ctrl: false,
        shift: false,
        alt: false,
        raw_char: None,
    };
    for part in s.split('+') {
        match part {
            "ctrl" | "control" => k.ctrl = true,
            "shift" => k.shift = true,
            "alt" | "opt" => k.alt = true,
            "enter" | "return" => k.kind = KeyKind::Enter,
            "esc" | "escape" => k.kind = KeyKind::Esc,
            "tab" => k.kind = KeyKind::Tab,
            "up" => k.kind = KeyKind::Up,
            "down" => k.kind = KeyKind::Down,
            "left" => k.kind = KeyKind::Left,
            "right" => k.kind = KeyKind::Right,
            "backspace" => k.kind = KeyKind::Backspace,
            "delete" | "del" => k.kind = KeyKind::Delete,
            "insert" => k.kind = KeyKind::Insert,
            "home" => k.kind = KeyKind::Home,
            "end" => k.kind = KeyKind::End,
            "pgup" => k.kind = KeyKind::PgUp,
            "pgdown" | "pgdn" => k.kind = KeyKind::PgDn,
            f if f.len() == 3 && f.starts_with('f') => {
                if let Ok(n) = f[1..].parse::<u8>() {
                    if (1..=12).contains(&n) {
                        k.kind = KeyKind::F(n);
                    }
                }
            }
            c if c.len() == 1 => {
                let ch = c.chars().next().unwrap();
                if ch.is_ascii_uppercase() {
                    k.shift = true;
                    k.kind = KeyKind::Ch(ch.to_ascii_lowercase());
                } else {
                    k.kind = KeyKind::Ch(ch);
                }
            }
            _ => return None,
        }
    }
    match k.kind {
        KeyKind::Ch('\0') => None,
        _ => Some(k),
    }
}

/// Normalize a crossterm key event to canonical form.
pub fn from_event(ev: &KeyEvent) -> Option<Key> {
    let (kind, raw) = match ev.code {
        KeyCode::Char(c) => (KeyKind::Ch(c.to_ascii_lowercase()), Some(c)),
        KeyCode::Enter => (KeyKind::Enter, None),
        KeyCode::Esc => (KeyKind::Esc, None),
        KeyCode::Tab => (KeyKind::Tab, None),
        KeyCode::Up => (KeyKind::Up, None),
        KeyCode::Down => (KeyKind::Down, None),
        KeyCode::Left => (KeyKind::Left, None),
        KeyCode::Right => (KeyKind::Right, None),
        KeyCode::Backspace => (KeyKind::Backspace, None),
        KeyCode::Delete => (KeyKind::Delete, None),
        KeyCode::Insert => (KeyKind::Insert, None),
        KeyCode::Home => (KeyKind::Home, None),
        KeyCode::End => (KeyKind::End, None),
        KeyCode::PageUp => (KeyKind::PgUp, None),
        KeyCode::PageDown => (KeyKind::PgDn, None),
        KeyCode::F(n) => (KeyKind::F(n), None),
        _ => return None,
    };
    Some(Key {
        kind,
        ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
        shift: ev.modifiers.contains(KeyModifiers::SHIFT),
        alt: ev.modifiers.contains(KeyModifiers::ALT),
        raw_char: raw,
    })
}

/// True if the key matches any of the given hotkey strings.
pub fn matches_any(hotkeys: &[String], k: &Key) -> bool {
    hotkeys.iter().any(|h| parse(h) == Some(*k))
}

/// The first configured hotkey of a list, as a display label (for modal hints).
pub fn first_label(hotkeys: &[String]) -> String {
    hotkeys.first().cloned().unwrap_or_default()
}

/// Char text for text-input widgets (None when the key is not printable).
pub fn input_char(k: &Key) -> Option<char> {
    if k.ctrl || k.alt {
        return None;
    }
    match k.kind {
        KeyKind::Ch(c) if c != '\0' => k.raw_char.or(Some(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn key(k: KeyEvent) -> Key {
        from_event(&k).unwrap()
    }

    #[test]
    fn parse_and_match() {
        assert_eq!(parse("q"), Some(Key::new(KeyKind::Ch('q'))));
        assert_eq!(parse("Q").unwrap().shift, true);
        assert_eq!(parse("ctrl+c").unwrap().ctrl, true);
        assert_eq!(parse("shift+left").unwrap().shift, true);
        assert_eq!(parse("pgdown"), Some(Key::new(KeyKind::PgDn)));
        assert_eq!(parse("backspace"), Some(Key::new(KeyKind::Backspace)));
        assert_eq!(parse(""), None);
        assert_eq!(parse("notakey"), None);
    }

    #[test]
    fn event_matching() {
        let hs: Vec<String> = vec!["ctrl+c".into(), "x".into()];
        assert!(matches_any(&hs, &key(ev(KeyCode::Char('c'), KeyModifiers::CONTROL))));
        assert!(matches_any(&hs, &key(ev(KeyCode::Char('x'), KeyModifiers::NONE))));
        assert!(!matches_any(&hs, &key(ev(KeyCode::Char('y'), KeyModifiers::NONE))));
        let hs: Vec<String> = vec!["Q".into()];
        assert!(matches_any(&hs, &key(ev(KeyCode::Char('Q'), KeyModifiers::SHIFT))));
        assert!(!matches_any(&hs, &key(ev(KeyCode::Char('q'), KeyModifiers::NONE))));
        // raw_char preserves case for text input
        assert_eq!(key(ev(KeyCode::Char('Q'), KeyModifiers::SHIFT)).raw_char, Some('Q'));
        assert_eq!(key(ev(KeyCode::Char('q'), KeyModifiers::NONE)).raw_char, Some('q'));
    }
}
