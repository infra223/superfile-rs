//! Dialogs/modals: Notify, SpfError, SortMenu, ThemeMenu, HelpMenu, TypingModal, first-use intro.
//! Exact port of superfile v1.6.0 internal/ui/{notify,spferror,sortmodel,thememenu,helpmenu} +
//! typing/first-use modals from internal/model_render.go.
//! Deviations: (1) lipgloss replaced by hand-rolled wrap/center/grow pipeline replicating lipgloss v2 +
//! ansi v0.11.7 semantics (verified from source); (2) multi-segment content lines are atomic (no wrap) and
//! clipped to inner width — Go would wrap the last column of the typing-modal location line only in
//! nerd-font mode; (3) BorderSet::plain() glyphs everywhere (house convention; theme border chars not
//! ported); (4) ThemeMenu::open receives (themes, current) from the app instead of reading the theme dir
//! itself; (5) SpfError drops the Go `title` field (stored but never rendered); (6) HelpMenu hotkey column
//! width = max plain display width + 2, right-aligned — equivalent to Go's byte-length-of-ANSI-string quirk
//! because ANSI overhead is constant across rows; (7) hotkeyWorkType field omitted (dead in Go render);
//! (8) char limit 156 enforced locally; (9) empty-theme StartIndex clamped to 0 (Go computes 1; renders
//! identically); (10) fuzzy filter uses crate::fuzzy (fzf-compatible).

use crate::config::{Hotkeys, Palette};
use crate::icons::Ui;
use crate::keys::{matches_any, Key};
use crate::render::{BorderSet, RBuf, St};
use crate::text_input::TextInput;
use crate::util::{plain_truncate, str_width, truncate_beginning, truncate_end};
use ratatui::style::Color;
use unicode_width::UnicodeWidthChar;

// ---- constants
pub const MODAL_W: usize = 60;
pub const MODAL_H: usize = 7;
pub const SORT_W: usize = 20;
pub const SORT_H: usize = 4;
pub const THEME_W: usize = 34;
pub const THEME_VISIBLE: usize = 7;
pub const CHAR_LIMIT: usize = 156;
pub const SORT_OPTIONS: [&str; 5] = ["Name", "Size", "Date Modified", "Type", "Natural"];

// ---- shared rendering helpers

/// One styled (text, fg, bg) piece of a logical content line.
#[derive(Clone)]
struct Seg {
    text: String,
    fg: Color,
    bg: Color,
}

/// ansi.Wrap semantics: buffer space runs, write them only right before the next non-space char and
/// only if they fit; a non-space char that does not fit closes the current physical line (pending
/// spaces dropped) and starts a new one; a word longer than the whole limit is hard-broken; leading
/// spaces preserved if they fit; trailing spaces kept only if they fit. limit==0 returns the line as-is.
fn word_wrap(s: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return vec![s.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col: usize = 0;
    let mut pending_spaces: usize = 0;
    for ch in s.chars() {
        if ch == ' ' {
            pending_spaces += 1;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + pending_spaces + w <= limit {
            for _ in 0..pending_spaces {
                current.push(' ');
            }
            col += pending_spaces;
            pending_spaces = 0;
            current.push(ch);
            col += w;
        } else {
            lines.push(std::mem::take(&mut current));
            pending_spaces = 0;
            current.push(ch);
            col = w;
        }
    }
    if col + pending_spaces <= limit {
        for _ in 0..pending_spaces {
            current.push(' ');
        }
    }
    lines.push(current);
    lines
}

/// Expand logical lines to physical lines: single-segment (or empty) lines are word-wrapped at the
/// inner width; multi-segment lines are atomic.
fn expand_lines(logical_lines: &[Vec<Seg>], cw: usize) -> Vec<Vec<Seg>> {
    let mut physical: Vec<Vec<Seg>> = Vec::new();
    for line in logical_lines {
        if line.len() <= 1 {
            if line.is_empty() {
                physical.push(Vec::new());
            } else {
                let seg = &line[0];
                for wl in word_wrap(&seg.text, cw) {
                    physical.push(vec![Seg {
                        text: wl,
                        fg: seg.fg,
                        bg: seg.bg,
                    }]);
                }
            }
        } else {
            physical.push(line.clone());
        }
    }
    physical
}

struct DrawerSetup {
    physical: Vec<Vec<Seg>>,
    box_h: usize,
    top: usize,
}

/// Common lipgloss box setup: expand lines, compute growth (`Height` is a minimum) and vertical padding.
fn setup_drawer(w: usize, h: usize, logical_lines: &[Vec<Seg>]) -> Option<DrawerSetup> {
    let cw = w.saturating_sub(2);
    let physical = expand_lines(logical_lines, cw);
    let ch = h.saturating_sub(2);
    let n = physical.len();
    let box_h = if n > ch { n + 2 } else { h };
    if w < 2 || box_h < 2 {
        return None;
    }
    let top = if n > ch { 0 } else { (ch - n) / 2 };
    Some(DrawerSetup {
        physical,
        box_h,
        top,
    })
}

/// Draw styled segments left-to-right from (cx, row), pre-clipping each segment so nothing is written
/// past `end_x` (put_str only clips at the buffer edge). Returns the x after the last drawn char.
fn draw_segs_clipped(buf: &mut RBuf, cx: usize, row: usize, end_x: usize, segs: &[Seg]) -> usize {
    let mut x = cx;
    for seg in segs {
        if x >= end_x {
            break;
        }
        let sw = str_width(&seg.text);
        if sw == 0 {
            continue;
        }
        let rem = end_x - x;
        let text = if sw > rem {
            plain_truncate(&seg.text, rem)
        } else {
            seg.text.clone()
        };
        if text.is_empty() {
            break;
        }
        x = buf.put_str(x, row, &text, St::fg_bg(seg.fg, seg.bg));
    }
    x
}

/// lipgloss Align(Center, Center) box: fill, border, per-line horizontal centering with clipping.
fn draw_center_center(
    buf: &mut RBuf,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    logical_lines: &[Vec<Seg>],
    pal: &Palette,
) {
    let Some(setup) = setup_drawer(w, h, logical_lines) else {
        return;
    };
    buf.fill(x, y, w, setup.box_h, St::fg_bg(pal.modal_bg, pal.modal_bg));
    let set = BorderSet::plain();
    buf.border(x, y, w, setup.box_h, set, pal.modal_border, pal.modal_bg);
    let cw = w.saturating_sub(2);
    let end_x = x + w - 1;
    for (i, segs) in setup.physical.iter().enumerate() {
        let row = y + 1 + setup.top + i;
        if row >= y + setup.box_h - 1 {
            continue;
        }
        let lw: usize = segs.iter().map(|s| str_width(&s.text)).sum();
        let left = if lw >= cw { 0 } else { (cw - lw) / 2 };
        let cx = x + 1 + left;
        let _ = draw_segs_clipped(buf, cx, row, end_x, segs);
    }
}

/// lipgloss Align(Left, Center) box (first-use modal): left-aligned, vertically centered, same growth.
fn draw_left_center(
    buf: &mut RBuf,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    logical_lines: &[Vec<Seg>],
    pal: &Palette,
) {
    let Some(setup) = setup_drawer(w, h, logical_lines) else {
        return;
    };
    buf.fill(x, y, w, setup.box_h, St::fg_bg(pal.modal_bg, pal.modal_bg));
    let set = BorderSet::plain();
    buf.border(x, y, w, setup.box_h, set, pal.modal_border, pal.modal_bg);
    let end_x = x + w - 1;
    for (i, segs) in setup.physical.iter().enumerate() {
        let row = y + 1 + setup.top + i;
        if row >= y + setup.box_h - 1 {
            continue;
        }
        let cx = x + 1;
        let _ = draw_segs_clipped(buf, cx, row, end_x, segs);
    }
}

/// lipgloss box with Width/Height but NO Align (top-left), growing, plus a GenerateFooterBorder
/// bottom-border row: bottom×repeat + middle_right + count, exactly inner width.
fn draw_top_left_grow(
    buf: &mut RBuf,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    logical_lines: &[Vec<Seg>],
    pal: &Palette,
    footer_count: &str,
) {
    let cw = w.saturating_sub(2);
    let physical = expand_lines(logical_lines, cw);
    let ch = h.saturating_sub(2);
    let n = physical.len();
    let box_h = if n > ch { n + 2 } else { h };
    if w < 2 || box_h < 2 {
        return;
    }
    buf.fill(x, y, w, box_h, St::fg_bg(pal.modal_bg, pal.modal_bg));
    let set = BorderSet::plain();
    buf.border(x, y, w, box_h, set, pal.modal_border, pal.modal_bg);
    let end_x = x + w - 1;
    for (i, segs) in physical.iter().enumerate() {
        let row = y + 1 + i;
        if row >= y + box_h - 1 {
            continue;
        }
        let cx = x + 1;
        let _ = draw_segs_clipped(buf, cx, row, end_x, segs);
    }
    let inner = w.saturating_sub(2);
    let count_w = str_width(footer_count);
    let repeat = inner.saturating_sub(count_w).saturating_sub(1);
    let mut s = String::new();
    for _ in 0..repeat {
        s.push_str(set.bottom);
    }
    s.push_str(set.middle_right);
    s.push_str(footer_count);
    buf.put_line(
        x + 1,
        y + box_h - 1,
        inner,
        &s,
        St::fg_bg(pal.modal_border, pal.modal_bg),
    );
}

/// The first hotkey string, as a whole (Go: `Hotkeys.X[0]`, e.g. "enter" or
/// "ctrl+c"), or "" when the list is empty.
fn first_hotkey(h: &[String]) -> &str {
    h.first().map(|s| s.as_str()).unwrap_or("")
}

/// Display-only Go filepath.Join-ish: join with "/" and minimal cleaning (collapse duplicate
/// separators, resolve "." and ".." segments, keep a leading "/" if present).
fn join_path(location: &str, value: &str) -> String {
    if value.is_empty() {
        return location.to_string();
    }
    let joined = format!("{}/{}", location, value);
    let leading_slash = joined.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            parts.pop();
        } else {
            parts.push(seg);
        }
    }
    let mut result = parts.join("/");
    if leading_slash {
        result = format!("/{}", result);
    }
    result
}

/// Port of common.GetHelpMenuHotkeyString: skip empty entries, " | " separator when i != 0 (i indexes
/// the ORIGINAL slice), " " rendered as "space".
fn hotkey_string(hotkeys: &[String]) -> String {
    let mut s = String::new();
    for (i, key) in hotkeys.iter().enumerate() {
        if key.is_empty() {
            continue;
        }
        if i != 0 {
            s.push_str(" | ");
        }
        let display = if key == " " { "space" } else { key.as_str() };
        s.push_str(display);
    }
    s
}

/// Enforce the 156-char buffer limit, clamping the cursor.
fn enforce_char_limit(input: &mut TextInput) {
    if input.buf.chars().count() > CHAR_LIMIT {
        let truncated: String = input.buf.chars().take(CHAR_LIMIT).collect();
        input.cursor = input.cursor.min(CHAR_LIMIT);
        input.buf = truncated;
    }
}

// ---- Notify

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    Rename,
    Delete,
    Quit,
    NoAction,
    PermanentDelete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyAction {
    None,
    Cancel,
    Confirm,
}

pub struct Notify {
    open: bool,
    title: String,
    content: String,
    action: ConfirmAction,
}

impl Notify {
    pub fn new() -> Self {
        Notify {
            open: false,
            title: String::new(),
            content: String::new(),
            action: ConfirmAction::NoAction,
        }
    }

    pub fn open(&mut self, title: String, content: String, action: ConfirmAction) {
        self.open = true;
        self.title = title;
        self.content = content;
        self.action = action;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn action(&self) -> ConfirmAction {
        self.action
    }

    /// isCancel = matches_any(cancel_typing) OR matches_any(quit); isConfirm =
    /// matches_any(confirm_typing); else None (modal stays open).
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> NotifyAction {
        let is_cancel = matches_any(&hk.cancel_typing, k) || matches_any(&hk.quit, k);
        let is_confirm = matches_any(&hk.confirm_typing, k);
        if !is_cancel && !is_confirm {
            return NotifyAction::None;
        }
        self.close();
        if is_cancel {
            NotifyAction::Cancel
        } else {
            NotifyAction::Confirm
        }
    }

    /// 60×7, center/center, border pal.modal_border.
    pub fn draw(&self, buf: &mut RBuf, x: usize, y: usize, pal: &Palette, hk: &Hotkeys) {
        let tip = if self.action == ConfirmAction::NoAction {
            vec![Seg {
                text: format!(" ({}) Okay ", first_hotkey(&hk.confirm_typing)),
                fg: pal.modal_confirm_fg,
                bg: pal.modal_confirm_bg,
            }]
        } else {
            vec![
                Seg {
                    text: format!(" ({}) Confirm ", first_hotkey(&hk.confirm_typing)),
                    fg: pal.modal_confirm_fg,
                    bg: pal.modal_confirm_bg,
                },
                Seg {
                    text: "           ".to_string(),
                    fg: pal.modal_fg,
                    bg: pal.modal_bg,
                },
                Seg {
                    text: format!(" ({}) Cancel ", first_hotkey(&hk.quit)),
                    fg: pal.modal_cancel_fg,
                    bg: pal.modal_cancel_bg,
                },
            ]
        };
        let lines: Vec<Vec<Seg>> = vec![
            vec![Seg {
                text: self.title.clone(),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            }],
            vec![],
            vec![Seg {
                text: self.content.clone(),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            }],
            vec![],
            tip,
        ];
        draw_center_center(buf, x, y, MODAL_W, MODAL_H, &lines, pal);
    }
}

// ---- SpfError

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpfErrorAction {
    None,
    Skip,
    Abort,
}

pub struct SpfError {
    open: bool,
    content: String,
    remaining: Vec<String>,
}

impl SpfError {
    pub fn new() -> Self {
        SpfError {
            open: false,
            content: String::new(),
            remaining: Vec::new(),
        }
    }

    pub fn open(&mut self, content: String, remaining: Vec<String>) {
        self.open = true;
        self.content = content;
        self.remaining = remaining;
    }

    /// open=false, return the stored remaining list and clear it (Go Close()).
    pub fn close(&mut self) -> Vec<String> {
        self.open = false;
        std::mem::take(&mut self.remaining)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn remaining(&self) -> &[String] {
        &self.remaining
    }

    /// is_abort = matches_any(quit); is_skip = matches_any(confirm_typing); else None.
    /// The action does NOT mutate state here; the app calls close() then applies.
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> SpfErrorAction {
        let is_abort = matches_any(&hk.quit, k);
        let is_skip = matches_any(&hk.confirm_typing, k);
        if !is_abort && !is_skip {
            return SpfErrorAction::None;
        }
        if is_abort {
            SpfErrorAction::Abort
        } else {
            SpfErrorAction::Skip
        }
    }

    /// 60×7, center/center.
    pub fn draw(&self, buf: &mut RBuf, x: usize, y: usize, pal: &Palette, hk: &Hotkeys) {
        let tip = vec![
            Seg {
                text: format!(" ({}) Skip ", first_hotkey(&hk.confirm_typing)),
                fg: pal.modal_confirm_fg,
                bg: pal.modal_confirm_bg,
            },
            Seg {
                text: "           ".to_string(),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            },
            Seg {
                text: format!(" ({}) Abort ", first_hotkey(&hk.quit)),
                fg: pal.modal_cancel_fg,
                bg: pal.modal_cancel_bg,
            },
        ];
        let lines: Vec<Vec<Seg>> = vec![
            vec![Seg {
                text: "Error".to_string(),
                fg: pal.error,
                bg: pal.modal_bg,
            }],
            vec![Seg {
                text: self.content.clone(),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            }],
            vec![],
            tip,
        ];
        draw_center_center(buf, x, y, MODAL_W, MODAL_H, &lines, pal);
    }
}

// ---- SortMenu

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortAction {
    None,
    Close,
    Confirm(usize),
}

pub struct SortMenu {
    open: bool,
    cursor: usize,
}

impl SortMenu {
    pub fn new() -> Self {
        SortMenu {
            open: false,
            cursor: 0,
        }
    }

    pub fn open(&mut self, sort_kind: usize) {
        self.cursor = sort_kind;
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.cursor = 0;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn list_up(&mut self) {
        self.cursor = (self.cursor + SORT_OPTIONS.len() - 1) % SORT_OPTIONS.len();
    }

    pub fn list_down(&mut self) {
        self.cursor = (self.cursor + 1) % SORT_OPTIONS.len();
    }

    /// open_sort_options_menu → Close; quit → Close; confirm → Confirm(cursor);
    /// list_up/list_down → navigate (None).
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> SortAction {
        if matches_any(&hk.open_sort_options_menu, k) {
            self.close();
            return SortAction::Close;
        }
        if matches_any(&hk.quit, k) {
            self.close();
            return SortAction::Close;
        }
        if matches_any(&hk.confirm, k) {
            return SortAction::Confirm(self.cursor);
        }
        if matches_any(&hk.list_up, k) {
            self.list_up();
            return SortAction::None;
        }
        if matches_any(&hk.list_down, k) {
            self.list_down();
            return SortAction::None;
        }
        SortAction::None
    }

    /// 20×4 declared, NO alignment (top-left), box GROWS (content = 8 lines → 20×10).
    pub fn draw(&self, buf: &mut RBuf, x: usize, y: usize, pal: &Palette, ui: &Ui) {
        let mut lines: Vec<Vec<Seg>> = vec![
            vec![Seg {
                text: " Sort Options".to_string(),
                fg: pal.hint,
                bg: pal.modal_bg,
            }],
            vec![],
        ];
        for (i, option) in SORT_OPTIONS.iter().enumerate() {
            let cursor_seg = if i == self.cursor {
                Seg {
                    text: ui.cursor.to_string(),
                    fg: pal.cursor,
                    bg: pal.file_panel_bg,
                }
            } else {
                Seg {
                    text: " ".to_string(),
                    fg: pal.modal_fg,
                    bg: pal.modal_bg,
                }
            };
            let option_seg = Seg {
                text: format!(" {}", option),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            };
            lines.push(vec![cursor_seg, option_seg]);
        }
        lines.push(vec![]); // trailing line (Go content ends with "\n")
        let count = format!("{}/{}", self.cursor + 1, SORT_OPTIONS.len());
        draw_top_left_grow(buf, x, y, SORT_W, SORT_H, &lines, pal, &count);
    }
}

// ---- ThemeMenu

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeAction {
    None,
    Close,
    Confirm(usize),
}

pub struct ThemeMenu {
    open: bool,
    cursor: usize,
    start_index: usize,
    themes: Vec<String>,
    current: String,
    err_msg: String,
    width: usize,
    height: usize,
}

impl ThemeMenu {
    pub fn new() -> Self {
        ThemeMenu {
            open: false,
            cursor: 0,
            start_index: 0,
            themes: Vec::new(),
            current: String::new(),
            err_msg: String::new(),
            width: THEME_W,
            height: 7,
        }
    }

    pub fn set_dimensions(&mut self, w: usize, h: usize) {
        self.width = w;
        self.height = h;
    }

    /// Mirrors Go Open(): store themes+current, err_msg = "", cursor = position of current
    /// (or 0 when absent/empty), update_start_index(), open = true.
    pub fn open(&mut self, themes: Vec<String>, current: String) {
        self.themes = themes;
        self.current = current;
        self.err_msg = String::new();
        self.cursor = self
            .themes
            .iter()
            .position(|t| t == &self.current)
            .unwrap_or(0);
        self.update_start_index();
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.cursor = 0;
        self.start_index = 0;
        self.err_msg = String::new();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// "" when cursor out of range.
    pub fn selected(&self) -> String {
        self.themes.get(self.cursor).cloned().unwrap_or_default()
    }

    pub fn list_up(&mut self) {
        if self.themes.is_empty() {
            return;
        }
        self.cursor = (self.cursor + self.themes.len() - 1) % self.themes.len();
        self.update_start_index();
        self.clear_error();
    }

    pub fn list_down(&mut self) {
        if self.themes.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.themes.len();
        self.update_start_index();
        self.clear_error();
    }

    pub fn set_error(&mut self, msg: String) {
        self.err_msg = msg;
    }

    pub fn clear_error(&mut self) {
        self.err_msg = String::new();
    }

    pub fn start_index(&self) -> usize {
        self.start_index
    }

    pub fn themes(&self) -> &[String] {
        &self.themes
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// open_theme_menu → Close; quit → Close; confirm → Confirm(cursor); list nav.
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> ThemeAction {
        if matches_any(&hk.open_theme_menu, k) {
            self.close();
            return ThemeAction::Close;
        }
        if matches_any(&hk.quit, k) {
            self.close();
            return ThemeAction::Close;
        }
        if matches_any(&hk.confirm, k) {
            return ThemeAction::Confirm(self.cursor);
        }
        if matches_any(&hk.list_up, k) {
            self.list_up();
            return ThemeAction::None;
        }
        if matches_any(&hk.list_down, k) {
            self.list_down();
            return ThemeAction::None;
        }
        ThemeAction::None
    }

    fn visible_items(&self) -> usize {
        if self.themes.len() < THEME_VISIBLE {
            self.themes.len()
        } else {
            THEME_VISIBLE
        }
    }

    /// Port of Go updateStartIndex (usize-ified; empty themes → start_index 0, note 9).
    fn update_start_index(&mut self) {
        let visible = self.visible_items();
        if visible == 0 {
            self.start_index = 0;
            return;
        }
        if self.cursor < self.start_index {
            self.start_index = self.cursor;
        } else if self.cursor >= self.start_index + visible {
            self.start_index = self.cursor + 1 - visible;
        }
        let max_start = self.themes.len().saturating_sub(visible);
        if max_start > 0 && self.start_index > max_start {
            self.start_index = max_start;
        }
    }

    /// 34×7 declared, top-left, box GROWS.
    pub fn draw(&self, buf: &mut RBuf, x: usize, y: usize, pal: &Palette, ui: &Ui) {
        let w = self.width;
        let h = self.height;
        let mut lines: Vec<Vec<Seg>> = vec![
            vec![Seg {
                text: " Theme Selection".to_string(),
                fg: pal.hint,
                bg: pal.modal_bg,
            }],
            vec![],
        ];
        if self.themes.is_empty() {
            lines.push(vec![Seg {
                text: " No theme files found".to_string(),
                fg: pal.error,
                bg: pal.modal_bg,
            }]);
        } else {
            let end = (self.start_index + self.visible_items()).min(self.themes.len());
            for i in self.start_index..end {
                let theme = &self.themes[i];
                let cursor_seg = if i == self.cursor {
                    Seg {
                        text: ui.cursor.to_string(),
                        fg: pal.cursor,
                        bg: pal.file_panel_bg,
                    }
                } else {
                    Seg {
                        text: " ".to_string(),
                        fg: pal.modal_fg,
                        bg: pal.modal_bg,
                    }
                };
                let name_max_width = w.saturating_sub(2).saturating_sub(1).saturating_sub(9);
                let mut name = format!(" {}", theme);
                if str_width(&name) > name_max_width {
                    name = truncate_end(&name, name_max_width, "\u{2026}");
                }
                let mut row_segs = vec![
                    cursor_seg,
                    Seg {
                        text: name.clone(),
                        fg: pal.modal_fg,
                        bg: pal.modal_bg,
                    },
                ];
                if *theme == self.current {
                    let pad = name_max_width.saturating_sub(str_width(&name));
                    row_segs.push(Seg {
                        text: " ".repeat(pad),
                        fg: pal.modal_fg,
                        bg: pal.modal_bg,
                    });
                    row_segs.push(Seg {
                        text: " (active)".to_string(),
                        fg: pal.correct,
                        bg: pal.modal_bg,
                    });
                }
                lines.push(row_segs);
            }
        }
        if !self.err_msg.is_empty() {
            lines.push(vec![]);
            let mut errline = format!(" {}", self.err_msg);
            let max_w = w.saturating_sub(2);
            if str_width(&errline) > max_w {
                errline = truncate_end(&errline, max_w, "\u{2026}");
            }
            lines.push(vec![Seg {
                text: errline,
                fg: pal.error,
                bg: pal.modal_bg,
            }]);
        }
        lines.push(vec![]); // trailing line (Go content ends with "\n")
        let count = if self.themes.is_empty() {
            "0/0".to_string()
        } else {
            format!("{}/{}", self.cursor + 1, self.themes.len())
        };
        draw_top_left_grow(buf, x, y, w, h, &lines, pal, &count);
    }
}

// ---- HelpMenu

#[derive(Debug, Clone, PartialEq)]
pub struct HotkeyRow {
    /// "" for hotkey rows.
    pub subtitle: String,
    /// empty for subtitle rows.
    pub hotkeys: Vec<String>,
    /// "" for subtitle rows.
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpMenuAction {
    None,
    Close,
}

pub struct HelpMenu {
    open: bool,
    width: usize,
    height: usize,
    render_index: usize,
    cursor: usize,
    data: Vec<HotkeyRow>,
    filtered: Vec<HotkeyRow>,
    input: TextInput,
    search_focused: bool,
}

impl HelpMenu {
    /// Builds the 52-row table from hk. cursor = 1, render_index = 0, filtered = data, closed.
    pub fn new(hk: &Hotkeys) -> Self {
        let mut data: Vec<HotkeyRow> = Vec::new();
        macro_rules! sub {
            ($t:expr) => {
                data.push(HotkeyRow {
                    subtitle: $t.to_string(),
                    hotkeys: Vec::new(),
                    description: String::new(),
                })
            };
        }
        macro_rules! row {
            ($hk:expr, $d:expr) => {
                data.push(HotkeyRow {
                    subtitle: String::new(),
                    hotkeys: $hk,
                    description: $d.to_string(),
                })
            };
        }
        sub!("General");
        row!(vec!["spf".to_string(), String::new()], "Open superfile");
        row!(hk.confirm.clone(), "Confirm your selection or typing");
        row!(hk.quit.clone(), "Quit typing, modal or superfile");
        row!(
            hk.cd_quit.clone(),
            "Quit superfile and change directory to current folder"
        );
        row!(hk.confirm_typing.clone(), "Confirm typing");
        row!(hk.cancel_typing.clone(), "Cancel typing");
        row!(hk.open_help_menu.clone(), "Open help menu (hotkey list)");
        row!(hk.open_command_line.clone(), "Open command line");
        row!(hk.open_spf_prompt.clone(), "Open SPF prompt");
        row!(hk.open_zoxide.clone(), "Open zoxide navigation");
        sub!("Panel navigation");
        row!(
            hk.create_new_file_panel.clone(),
            "Create new file panel"
        );
        row!(
            hk.split_file_panel.clone(),
            "Split file panel (open new panel in same directory)"
        );
        row!(hk.close_file_panel.clone(), "Close the focused file panel");
        row!(
            hk.toggle_file_preview_panel.clone(),
            "Toggle file preview panel"
        );
        row!(
            hk.open_sort_options_menu.clone(),
            "Open sort options menu"
        );
        row!(hk.open_theme_menu.clone(), "Open theme selection menu");
        row!(hk.toggle_reverse_sort.clone(), "Toggle reverse sort");
        row!(hk.toggle_footer.clone(), "Toggle footer");
        row!(hk.next_file_panel.clone(), "Focus on the next file panel");
        row!(
            hk.previous_file_panel.clone(),
            "Focus on the previous file panel"
        );
        row!(
            hk.focus_on_process_bar.clone(),
            "Focus on the processbar panel"
        );
        row!(hk.focus_on_sidebar.clone(), "Focus on the sidebar");
        row!(
            hk.focus_on_metadata.clone(),
            "Focus on the metadata panel"
        );
        sub!("Panel movement");
        row!(hk.list_up.clone(), "Up");
        row!(hk.list_down.clone(), "Down");
        row!(hk.page_up.clone(), "Page up");
        row!(hk.page_down.clone(), "Page down");
        row!(hk.parent_directory.clone(), "Return to parent folder");
        row!(
            hk.file_panel_select_all_items.clone(),
            "Select all items in focused file panel"
        );
        row!(
            hk.file_panel_select_mode_items_select_up.clone(),
            "Select up from your cursor"
        );
        row!(
            hk.file_panel_select_mode_items_select_down.clone(),
            "Select down from your cursor"
        );
        row!(hk.toggle_dot_file.clone(), "Toggle dot file display");
        row!(hk.search_bar.clone(), "Toggle active search bar");
        row!(
            hk.change_panel_mode.clone(),
            "Change between selection mode or normal mode"
        );
        row!(
            hk.pinned_directory.clone(),
            "Pin or Unpin folder to sidebar (can be auto saved)"
        );
        sub!("File operations");
        row!(
            hk.file_panel_item_create.clone(),
            "Create file or folder (end with / to create a folder)"
        );
        row!(hk.file_panel_item_rename.clone(), "Rename file or folder");
        row!(
            hk.copy_items.clone(),
            "Copy selected items to the clipboard"
        );
        row!(
            hk.cut_items.clone(),
            "Cut selected items to the clipboard"
        );
        row!(
            hk.paste_items.clone(),
            "Paste clipboard items into the current file panel"
        );
        row!(hk.delete_items.clone(), "Delete selected items");
        row!(
            hk.permanently_delete_items.clone(),
            "Permanently delete selected items"
        );
        row!(
            hk.copy_path.clone(),
            "Copy current or selected file/directory paths"
        );
        row!(
            hk.copy_present_working_directory.clone(),
            "Copy current working directory"
        );
        row!(hk.extract_file.clone(), "Extract compressed file");
        row!(
            hk.compress_file.clone(),
            "Zip file or folder to .zip file"
        );
        row!(
            hk.open_file_with_editor.clone(),
            "Open file with your default editor"
        );
        row!(
            hk.open_current_directory_with_editor.clone(),
            "Open current directory with default editor"
        );
        let filtered = data.clone();
        HelpMenu {
            open: false,
            width: 0,
            height: 0,
            render_index: 0,
            cursor: 1,
            data,
            filtered,
            input: TextInput::new(),
            search_focused: false,
        }
    }

    /// input.width = w - 6 (2 border + 1 left padding + 2 searchbar prompt icon + 1 mystery char).
    pub fn set_dimensions(&mut self, w: usize, h: usize) {
        self.width = w;
        self.height = h;
        self.input.set_width(w.saturating_sub(6));
    }

    /// TOGGLE: if open → close(); else filtered = data.clone(), open = true.
    pub fn open(&mut self) {
        if self.open {
            self.close();
            return;
        }
        self.filtered = self.data.clone();
        self.open = true;
    }

    pub fn close(&mut self) {
        self.input = TextInput::new();
        self.search_focused = false;
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// When search focused: confirm_typing/cancel_typing → blur (None); any other key → feed to
    /// input (enforce CHAR_LIMIT chars), then filter(value) (None). When not focused: list_up →
    /// ListUp; list_down → ListDown; quit → Close; search_bar → focus search (None); else None.
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> HelpMenuAction {
        if self.search_focused {
            if matches_any(&hk.confirm_typing, k) || matches_any(&hk.cancel_typing, k) {
                self.search_focused = false;
                return HelpMenuAction::None;
            }
            self.input.handle_key(k);
            enforce_char_limit(&mut self.input);
            let value = self.input.value().to_string();
            self.filter(&value);
            return HelpMenuAction::None;
        }
        if matches_any(&hk.list_up, k) {
            self.list_up();
            return HelpMenuAction::None;
        }
        if matches_any(&hk.list_down, k) {
            self.list_down();
            return HelpMenuAction::None;
        }
        if matches_any(&hk.quit, k) {
            self.close();
            return HelpMenuAction::Close;
        }
        if matches_any(&hk.search_bar, k) {
            self.search_focused = true;
            return HelpMenuAction::None;
        }
        HelpMenuAction::None
    }

    pub fn list_up(&mut self) {
        if self.cursor > 1 {
            self.cursor -= 1;
            if self.cursor < self.render_index {
                self.render_index = self.cursor;
            }
            if !self.filtered[self.cursor].subtitle.is_empty() {
                self.cursor -= 1;
            }
        } else {
            self.cursor = self.filtered.len().saturating_sub(1);
            self.render_index = self
                .filtered
                .len()
                .saturating_sub(self.height.saturating_sub(4));
        }
    }

    pub fn list_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.cursor < self.filtered.len() - 1 {
            let mut next = self.cursor + 1;
            while next < self.filtered.len() && !self.filtered[next].subtitle.is_empty() {
                next += 1;
            }
            if next >= self.filtered.len() {
                self.cursor = 1;
                self.render_index = 0;
                return;
            }
            self.cursor = next;
            if self.cursor > self.render_index + self.height.saturating_sub(5) {
                self.render_index += 1;
            }
            let bottom = self
                .filtered
                .len()
                .saturating_sub(self.height.saturating_sub(4));
            if self.render_index > bottom {
                self.render_index = bottom;
            }
        } else {
            self.cursor = 1;
            self.render_index = 0;
        }
    }

    /// fuzzySearch + removeOrphanSections: keep all subtitles and matched non-subtitle rows in
    /// original order, then drop subtitles not followed by a hotkey row; reset cursor/render_index.
    fn filter(&mut self, query: &str) {
        let mut filtered: Vec<HotkeyRow> = Vec::new();
        for item in &self.data {
            if !item.subtitle.is_empty() {
                filtered.push(item.clone());
                continue;
            }
            let haystack = item.hotkeys.join(" ") + " " + &item.description;
            let matched = query.is_empty() || crate::fuzzy::score(query, &haystack) > 0;
            if matched {
                filtered.push(item.clone());
            }
        }
        let mut result: Vec<HotkeyRow> = Vec::new();
        for i in 0..filtered.len() {
            if !filtered[i].subtitle.is_empty() {
                if i + 1 < filtered.len() && filtered[i + 1].subtitle.is_empty() {
                    result.push(filtered[i].clone());
                }
            } else {
                result.push(filtered[i].clone());
            }
        }
        self.filtered = result;
        if self.filtered.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = 1;
        }
        self.render_index = 0;
    }

    /// Box w×h (exact, no growth — content is always exactly h-2 lines).
    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        pal: &Palette,
        ui: &Ui,
        hk: &Hotkeys,
    ) {
        let w = self.width;
        let h = self.height;
        if w < 2 || h < 2 {
            return;
        }
        buf.fill(x, y, w, h, St::fg_bg(pal.modal_bg, pal.modal_bg));
        let set = BorderSet::plain();
        let subtitle_count = self
            .filtered
            .iter()
            .filter(|r| !r.subtitle.is_empty())
            .count();
        let total = self.filtered.len().saturating_sub(subtitle_count);
        let cursor_titles_before = self
            .filtered
            .iter()
            .take(self.cursor)
            .filter(|r| !r.subtitle.is_empty())
            .count();
        let current = if self.filtered.is_empty() {
            0
        } else {
            self.cursor + 1 - cursor_titles_before
        };
        let info = format!("{}/{}", current, total);
        buf.border_info(
            x,
            y,
            w,
            h,
            set,
            pal.modal_border,
            pal.modal_bg,
            &[info.as_str()],
        );

        let cx = x + 1;
        let end_x = x + w - 1;

        // Row 0: " " + search bar
        let prompt = format!("{}{}", ui.search, ui.space);
        let view = if self.input.value().is_empty() {
            let first = hk
                .search_bar
                .first()
                .cloned()
                .unwrap_or_default();
            let ph = format!("({}) Type something", first);
            if self.search_focused {
                format!("│{}", ph)
            } else {
                ph
            }
        } else {
            self.input.view(self.search_focused)
        };
        let line0 = vec![
            Seg {
                text: " ".to_string(),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            },
            Seg {
                text: prompt,
                fg: pal.file_panel_top_dir_icon,
                bg: pal.file_panel_bg,
            },
            Seg {
                text: view,
                fg: pal.file_panel_fg,
                bg: pal.file_panel_bg,
            },
        ];
        let _ = draw_segs_clipped(buf, cx, y + 1, end_x, &line0);

        // Row 1: blank (already modal_bg)

        // Rows 2..2+(h-4)
        // col_w = max over VISIBLE non-subtitle rows of (display_width(hotkey_str) + 1) + 1
        let visible_end = (self.render_index + self.height.saturating_sub(4))
            .min(self.filtered.len());
        let mut max_hotkey_w = 0usize;
        let mut any_visible = false;
        for i in self.render_index..visible_end {
            if !self.filtered[i].subtitle.is_empty() {
                continue;
            }
            any_visible = true;
            let hs = hotkey_string(&self.filtered[i].hotkeys);
            let hw = str_width(&hs);
            if hw > max_hotkey_w {
                max_hotkey_w = hw;
            }
        }
        let col_w = if any_visible {
            max_hotkey_w + 2
        } else {
            1
        };
        // value_length over ALL filtered non-subtitle rows
        let mut max_key_length = 0usize;
        for row in &self.filtered {
            if !row.subtitle.is_empty() {
                continue;
            }
            let keys_w: usize = row.hotkeys.iter().map(|k| str_width(k)).sum();
            let sep = row.hotkeys.len().saturating_sub(1) * 3;
            let total_len = keys_w + sep;
            if total_len > max_key_length {
                max_key_length = total_len;
            }
        }
        let mut value_length = w
            .saturating_sub(max_key_length)
            .saturating_sub(2);
        let half = w / 2;
        if value_length < half {
            value_length = half.saturating_sub(2);
        }

        let rows_to_draw = self.height.saturating_sub(4);
        for r in 0..rows_to_draw {
            let row = y + 3 + r;
            if row >= y + h - 1 {
                break;
            }
            let i = self.render_index + r;
            if i >= self.filtered.len() {
                break; // remaining rows stay blank (modal_bg)
            }
            let item = &self.filtered[i];
            if !item.subtitle.is_empty() {
                let seg = Seg {
                    text: format!(" {}", item.subtitle),
                    fg: pal.help_menu_title,
                    bg: pal.modal_bg,
                };
                let _ = draw_segs_clipped(buf, cx, row, end_x, std::slice::from_ref(&seg));
            } else {
                let is_cursor = self.cursor == i;
                let cursor_seg = if is_cursor {
                    Seg {
                        text: format!("{} ", ui.cursor),
                        fg: pal.cursor,
                        bg: pal.file_panel_bg,
                    }
                } else {
                    Seg {
                        text: "  ".to_string(),
                        fg: pal.modal_fg,
                        bg: pal.modal_bg,
                    }
                };
                let hs = hotkey_string(&item.hotkeys);
                let hs_plus = format!("{} ", hs);
                let hs_w = str_width(&hs_plus);
                let left_pad = col_w.saturating_sub(hs_w);
                let pad_seg = Seg {
                    text: " ".repeat(left_pad),
                    fg: pal.modal_fg,
                    bg: pal.modal_bg,
                };
                let hotkey_seg = Seg {
                    text: hs_plus,
                    fg: pal.help_menu_hotkey,
                    bg: pal.modal_bg,
                };
                let desc = truncate_end(&item.description, value_length, "...");
                let desc_seg = Seg {
                    text: desc,
                    fg: pal.modal_fg,
                    bg: pal.modal_bg,
                };
                let row_segs = [cursor_seg, pad_seg, hotkey_seg, desc_seg];
                let _ = draw_segs_clipped(buf, cx, row, end_x, &row_segs);
            }
        }
    }
}

// ---- TypingModal

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingAction {
    None,
    Cancel,
    Confirm,
}

pub struct TypingModal {
    open: bool,
    location: String,
    input: TextInput,
}

impl TypingModal {
    pub fn new() -> Self {
        TypingModal {
            open: false,
            location: String::new(),
            input: TextInput::new(),
        }
    }

    pub fn open(&mut self, location: String) {
        self.input = TextInput::new();
        self.input.set_width(50); // ModalWidth - 10
        self.location = location;
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn value(&self) -> &str {
        self.input.value()
    }

    pub fn location(&self) -> &str {
        &self.location
    }

    /// cancel_typing → close + Cancel; confirm_typing → Confirm (DO NOT close); any other key →
    /// feed input (enforce CHAR_LIMIT chars) + None.
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> TypingAction {
        if matches_any(&hk.cancel_typing, k) {
            self.close();
            return TypingAction::Cancel;
        }
        if matches_any(&hk.confirm_typing, k) {
            return TypingAction::Confirm;
        }
        self.input.handle_key(k);
        enforce_char_limit(&mut self.input);
        TypingAction::None
    }

    /// 60×7, center/center.
    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        pal: &Palette,
        ui: &Ui,
        hk: &Hotkeys,
    ) {
        // Line 1: location (two segments, atomic)
        let preview = join_path(&self.location, self.input.value());
        let path = truncate_beginning(&preview, MODAL_W.saturating_sub(4), "...");
        let icon_seg = Seg {
            // Go: " " + icon.Directory + icon.Space (Space = " " nerd, "" non-nerd).
            text: format!(" {}{}", ui.directory, ui.space),
            fg: pal.file_panel_top_dir_icon,
            bg: pal.file_panel_bg,
        };
        let path_seg = Seg {
            text: path,
            fg: pal.file_panel_top_path,
            bg: pal.file_panel_bg,
        };
        // Line 2: input
        let input_view = if self.input.value().is_empty() {
            format!("│Add \"/\" transcend folders")
        } else {
            self.input.view(true)
        };
        let input_seg = Seg {
            text: input_view,
            fg: pal.modal_fg,
            bg: pal.modal_bg,
        };
        // Line 4: tip
        let tip = vec![
            Seg {
                text: format!(" ({}) Create ", first_hotkey(&hk.confirm_typing)),
                fg: pal.modal_confirm_fg,
                bg: pal.modal_confirm_bg,
            },
            Seg {
                text: "           ".to_string(),
                fg: pal.modal_fg,
                bg: pal.modal_bg,
            },
            Seg {
                text: format!(" ({}) Cancel ", first_hotkey(&hk.cancel_typing)),
                fg: pal.modal_cancel_fg,
                bg: pal.modal_cancel_bg,
            },
        ];
        let lines: Vec<Vec<Seg>> = vec![
            vec![icon_seg, path_seg],
            vec![input_seg],
            vec![],
            tip,
        ];
        draw_center_center(buf, x, y, MODAL_W, MODAL_H, &lines, pal);
    }
}

// ---- First-use intro modal

/// First-use intro modal. (w, h) = helpmenu dimensions. Box w×h, border pal.modal_border,
/// bg modal_bg, Align(Left, Center) — content LEFT-aligned, vertically centered (same growth
/// rule when wrapped lines > h - 2).
pub fn draw_first_use(
    buf: &mut RBuf,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    pal: &Palette,
) {
    let t = pal.sidebar_title;
    let m = pal.modal_fg;
    let e = pal.error;
    let bg = pal.modal_bg;
    let lines: Vec<Vec<Seg>> = vec![
        vec![Seg {
            text: " Thanks for using superfile!!".to_string(),
            fg: t,
            bg,
        }],
        vec![Seg {
            text: " You can read the following information before starting to use it!".to_string(),
            fg: m,
            bg,
        }],
        vec![],
        vec![Seg {
            text: "  ** Very importantly ** If you are a Vim/Nvim user, go to:".to_string(),
            fg: e,
            bg,
        }],
        vec![Seg {
            text: "  https://superfile.dev/configure/custom-hotkeys/ to change your hotkey settings!"
                .to_string(),
            fg: e,
            bg,
        }],
        vec![],
        vec![
            Seg {
                text: "  (1)".to_string(),
                fg: t,
                bg,
            },
            Seg {
                text: " If this is your first time, make sure you read:".to_string(),
                fg: m,
                bg,
            },
        ],
        vec![Seg {
            text: "      https://superfile.dev/getting-started/tutorial/".to_string(),
            fg: m,
            bg,
        }],
        vec![],
        vec![
            Seg {
                text: "  (2)".to_string(),
                fg: t,
                bg,
            },
            Seg {
                text: " If you forget the relevant keys during use,".to_string(),
                fg: m,
                bg,
            },
        ],
        vec![Seg {
            text: "      you can press \"?\" (shift+/) at any time to query the keys!".to_string(),
            fg: m,
            bg,
        }],
        vec![],
        vec![
            Seg {
                text: "  (3)".to_string(),
                fg: t,
                bg,
            },
            Seg {
                text: " For more customization you can refer to:".to_string(),
                fg: m,
                bg,
            },
        ],
        vec![Seg {
            text: "      https://superfile.dev/".to_string(),
            fg: m,
            bg,
        }],
        vec![],
        vec![
            Seg {
                text: "  (4)".to_string(),
                fg: t,
                bg,
            },
            Seg {
                text: " Thank you again for using superfile.".to_string(),
                fg: m,
                bg,
            },
        ],
        vec![Seg {
            text: "      If you have any questions, please feel free to ask at:".to_string(),
            fg: m,
            bg,
        }],
        vec![Seg {
            text: "      https://github.com/yorukot/superfile".to_string(),
            fg: m,
            bg,
        }],
        vec![Seg {
            text: "      Of course, you can always open a new issue to share your idea ".to_string(),
            fg: m,
            bg,
        }],
        vec![Seg {
            text: "      or report a bug!".to_string(),
            fg: m,
            bg,
        }],
        vec![],
    ];
    draw_left_center(buf, x, y, w, h, &lines, pal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyKind;

    fn hk() -> Hotkeys {
        Hotkeys {
            confirm: vec!["enter".into()],
            cd_quit: vec![],
            quit: vec!["q".into()],
            list_down: vec!["j".into()],
            list_up: vec!["k".into()],
            page_down: vec![],
            page_up: vec![],
            close_file_panel: vec![],
            create_new_file_panel: vec![],
            next_file_panel: vec![],
            open_sort_options_menu: vec!["s".into()],
            pinned_directory: vec![],
            previous_file_panel: vec![],
            split_file_panel: vec![],
            toggle_file_preview_panel: vec![],
            toggle_reverse_sort: vec![],
            focus_on_metadata: vec![],
            focus_on_process_bar: vec![],
            focus_on_sidebar: vec![],
            file_panel_item_create: vec![],
            file_panel_item_rename: vec![],
            copy_items: vec![],
            cut_items: vec![],
            delete_items: vec![],
            paste_items: vec![],
            permanently_delete_items: vec![],
            compress_file: vec![],
            extract_file: vec![],
            open_current_directory_with_editor: vec![],
            open_file_with_editor: vec![],
            change_panel_mode: vec![],
            copy_path: vec![],
            copy_present_working_directory: vec![],
            open_command_line: vec![],
            open_help_menu: vec![],
            open_spf_prompt: vec![],
            open_theme_menu: vec!["t".into()],
            open_zoxide: vec![],
            toggle_dot_file: vec![],
            toggle_footer: vec![],
            confirm_typing: vec!["enter".into()],
            cancel_typing: vec!["esc".into()],
            parent_directory: vec![],
            search_bar: vec!["/".into()],
            file_panel_select_mode_items_select_down: vec![],
            file_panel_select_mode_items_select_up: vec![],
            file_panel_select_all_items: vec![],
        }
    }

    #[test]
    fn word_wrap_short_unchanged() {
        assert_eq!(word_wrap("hi", 10), vec!["hi".to_string()]);
    }

    #[test]
    fn word_wrap_wraps_at_limit() {
        assert_eq!(
            word_wrap("hello world", 6),
            vec!["hello".to_string(), "world".to_string()]
        );
    }

    #[test]
    fn word_wrap_hard_breaks_long_word() {
        assert_eq!(
            word_wrap("abcdefghij", 3),
            vec![
                "abc".to_string(),
                "def".to_string(),
                "ghi".to_string(),
                "j".to_string()
            ]
        );
    }

    #[test]
    fn word_wrap_trailing_spaces_kept_when_fit() {
        assert_eq!(word_wrap("hello  ", 7), vec!["hello  ".to_string()]);
    }

    #[test]
    fn word_wrap_trailing_spaces_dropped_when_no_fit() {
        assert_eq!(word_wrap("hello   ", 7), vec!["hello".to_string()]);
    }

    #[test]
    fn word_wrap_leading_spaces_preserved() {
        assert_eq!(
            word_wrap("  hello", 10),
            vec!["  hello".to_string()]
        );
    }

    #[test]
    fn notify_handle_key() {
        let h = hk();
        let mut n = Notify::new();
        n.open("T".into(), "C".into(), ConfirmAction::Delete);
        assert!(n.is_open());
        assert_eq!(n.action(), ConfirmAction::Delete);
        // cancel via quit
        assert_eq!(
            n.handle_key(&Key::new(KeyKind::Ch('q')), &h),
            NotifyAction::Cancel
        );
        assert!(!n.is_open());
        // cancel via cancel_typing (esc)
        n.open("T".into(), "C".into(), ConfirmAction::Delete);
        assert_eq!(
            n.handle_key(&Key::new(KeyKind::Esc), &h),
            NotifyAction::Cancel
        );
        assert!(!n.is_open());
        // confirm via confirm_typing (enter)
        n.open("T".into(), "C".into(), ConfirmAction::Delete);
        assert_eq!(
            n.handle_key(&Key::new(KeyKind::Enter), &h),
            NotifyAction::Confirm
        );
        assert!(!n.is_open());
        // other key: None, stays open
        n.open("T".into(), "C".into(), ConfirmAction::Delete);
        assert_eq!(
            n.handle_key(&Key::new(KeyKind::Ch('z')), &h),
            NotifyAction::None
        );
        assert!(n.is_open());
    }

    #[test]
    fn spferror_handle_key_and_close() {
        let h = hk();
        let mut e = SpfError::new();
        e.open("err".into(), vec!["a".into(), "b".into()]);
        assert!(e.is_open());
        assert_eq!(e.remaining(), &["a".to_string(), "b".to_string()]);
        assert_eq!(
            e.handle_key(&Key::new(KeyKind::Enter), &h),
            SpfErrorAction::Skip
        );
        assert_eq!(
            e.handle_key(&Key::new(KeyKind::Ch('q')), &h),
            SpfErrorAction::Abort
        );
        assert_eq!(
            e.handle_key(&Key::new(KeyKind::Ch('z')), &h),
            SpfErrorAction::None
        );
        // handle_key does not mutate state
        assert!(e.is_open());
        assert_eq!(e.remaining(), &["a".to_string(), "b".to_string()]);
        // close returns remaining and empties
        let rem = e.close();
        assert_eq!(rem, vec!["a".to_string(), "b".to_string()]);
        assert!(!e.is_open());
        assert!(e.remaining().is_empty());
    }

    #[test]
    fn sortmenu_navigation_and_keys() {
        let h = hk();
        let mut s = SortMenu::new();
        assert!(!s.is_open());
        s.open(2);
        assert!(s.is_open());
        assert_eq!(s.cursor(), 2);
        s.list_down();
        assert_eq!(s.cursor(), 3);
        s.list_down();
        assert_eq!(s.cursor(), 4);
        s.list_down();
        assert_eq!(s.cursor(), 0); // wraps
        s.list_up();
        assert_eq!(s.cursor(), 4); // wraps
        assert_eq!(
            s.handle_key(&Key::new(KeyKind::Enter), &h),
            SortAction::Confirm(4)
        );
        assert_eq!(
            s.handle_key(&Key::new(KeyKind::Ch('k')), &h),
            SortAction::None
        );
        assert_eq!(s.cursor(), 3);
        assert_eq!(
            s.handle_key(&Key::new(KeyKind::Ch('q')), &h),
            SortAction::Close
        );
        assert!(!s.is_open());
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn thememenu_open_nav_windowing() {
        let themes: Vec<String> = (0..20).map(|i| format!("theme{}", i)).collect();
        let mut t = ThemeMenu::new();
        t.open(themes.clone(), "theme5".into());
        assert!(t.is_open());
        assert_eq!(t.cursor(), 5);
        assert_eq!(t.start_index(), 0);
        assert_eq!(t.selected(), "theme5");
        for _ in 0..8 {
            t.list_down();
        }
        assert_eq!(t.cursor(), 13);
        assert_eq!(t.start_index(), 7);
        for _ in 0..6 {
            t.list_down();
        }
        assert_eq!(t.cursor(), 19);
        assert_eq!(t.start_index(), 13); // 20 - 7
        // wrap up from 0
        let mut t3 = ThemeMenu::new();
        t3.open(themes.clone(), "theme0".into());
        assert_eq!(t3.cursor(), 0);
        t3.list_up();
        assert_eq!(t3.cursor(), 19);
        // empty themes
        let mut t2 = ThemeMenu::new();
        t2.open(vec![], String::new());
        assert_eq!(t2.selected(), "");
        assert_eq!(t2.start_index(), 0);
        t2.list_down(); // no-op when empty
        assert_eq!(t2.cursor(), 0);
        t2.set_error("boom".into());
        assert!(!t2.err_msg.is_empty());
        t2.clear_error();
        assert!(t2.err_msg.is_empty());
    }

    #[test]
    fn helpmenu_data_table_shape() {
        let h = hk();
        let m = HelpMenu::new(&h);
        assert_eq!(m.filtered.len(), 52);
        let subtitles = m
            .filtered
            .iter()
            .filter(|r| !r.subtitle.is_empty())
            .count();
        assert_eq!(subtitles, 4);
        assert_eq!(m.cursor, 1);
        assert_eq!(m.render_index, 0);
        assert!(!m.is_open());
    }

    #[test]
    fn helpmenu_filter_empty_query_keeps_all() {
        let h = hk();
        let mut m = HelpMenu::new(&h);
        m.set_dimensions(90, 30);
        m.open();
        assert!(m.is_open());
        assert_eq!(m.input.width, 84); // 90 - 6
        let total_before = m.filtered.len();
        m.filter("");
        assert_eq!(m.filtered.len(), total_before);
        assert_eq!(m.cursor, 1);
        assert_eq!(m.render_index, 0);
    }

    #[test]
    fn helpmenu_filter_no_match_removes_orphans() {
        let h = hk();
        let mut m = HelpMenu::new(&h);
        m.set_dimensions(90, 30);
        m.open();
        m.filter("zzzzz_nomatch_zzz");
        assert!(m.filtered.is_empty());
        assert_eq!(m.cursor, 0);
        assert_eq!(m.render_index, 0);
    }

    #[test]
    fn helpmenu_nav_skips_subtitles_and_wraps() {
        let h = hk();
        let mut m = HelpMenu::new(&h);
        m.set_dimensions(90, 30);
        m.open();
        assert_eq!(m.cursor, 1);
        assert!(m.filtered[1].subtitle.is_empty());
        m.list_down();
        assert_eq!(m.cursor, 2);
        assert!(m.filtered[2].subtitle.is_empty());
        m.list_up();
        assert_eq!(m.cursor, 1);
        // cursor == 1 → wraps to bottom
        m.list_up();
        assert_eq!(m.cursor, m.filtered.len() - 1);
        assert!(m.filtered[m.cursor].subtitle.is_empty());
    }

    #[test]
    fn helpmenu_border_info_count_math() {
        let h = hk();
        let mut m = HelpMenu::new(&h);
        m.set_dimensions(90, 30);
        m.open();
        let subtitle_count = m
            .filtered
            .iter()
            .filter(|r| !r.subtitle.is_empty())
            .count();
        let total = m.filtered.len() - subtitle_count;
        let cursor_titles_before = m
            .filtered
            .iter()
            .take(m.cursor)
            .filter(|r| !r.subtitle.is_empty())
            .count();
        let current = m.cursor + 1 - cursor_titles_before;
        assert_eq!(current, 1);
        assert_eq!(total, 48);
    }

    #[test]
    fn helpmenu_search_focus_and_input() {
        let h = hk();
        let mut m = HelpMenu::new(&h);
        m.set_dimensions(90, 30);
        m.open();
        // focus search via search_bar key
        assert_eq!(
            m.handle_key(&Key::new(KeyKind::Ch('/')), &h),
            HelpMenuAction::None
        );
        assert!(m.search_focused);
        // typing a char feeds the input and filters
        assert_eq!(
            m.handle_key(&Key::new(KeyKind::Ch('u')), &h),
            HelpMenuAction::None
        );
        assert_eq!(m.input.value(), "u");
        // blur via confirm_typing
        assert_eq!(
            m.handle_key(&Key::new(KeyKind::Enter), &h),
            HelpMenuAction::None
        );
        assert!(!m.search_focused);
        // quit closes
        assert_eq!(
            m.handle_key(&Key::new(KeyKind::Ch('q')), &h),
            HelpMenuAction::Close
        );
        assert!(!m.is_open());
    }

    #[test]
    fn typingmodal_keys_and_value() {
        let h = hk();
        let mut t = TypingModal::new();
        assert!(!t.is_open());
        t.open("/home/user".into());
        assert!(t.is_open());
        assert_eq!(t.location(), "/home/user");
        assert_eq!(t.value(), "");
        // typing chars land in the value
        assert_eq!(
            t.handle_key(&Key::new(KeyKind::Ch('a')), &h),
            TypingAction::None
        );
        assert_eq!(
            t.handle_key(&Key::new(KeyKind::Ch('b')), &h),
            TypingAction::None
        );
        assert_eq!(t.value(), "ab");
        // confirm → Confirm, value preserved, stays open
        assert_eq!(
            t.handle_key(&Key::new(KeyKind::Enter), &h),
            TypingAction::Confirm
        );
        assert_eq!(t.value(), "ab");
        assert!(t.is_open());
        // cancel → closes + Cancel
        assert_eq!(
            t.handle_key(&Key::new(KeyKind::Esc), &h),
            TypingAction::Cancel
        );
        assert!(!t.is_open());
    }

    #[test]
    fn join_path_cleaning() {
        assert_eq!(join_path("/home/user", "a/b"), "/home/user/a/b");
        assert_eq!(join_path("/home/user/", "a//b"), "/home/user/a/b");
        assert_eq!(join_path("/home/user", ""), "/home/user");
        assert_eq!(join_path("/home/user", "a/../b"), "/home/user/b");
        assert_eq!(join_path("rel", "x"), "rel/x");
        assert_eq!(join_path("/home", "."), "/home");
    }
}
