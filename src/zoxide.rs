//! Zoxide directory-jump modal.
//!
//! Rust port of the Go version's `src/internal/ui/zoxide` package
//! (`model.go`, `utils.go`, `navigation.go`, `render.go`, `consts.go`,
//! `type.go`). The actual zoxide querying is done out-of-process by the app
//! (the component only holds results); this mirrors the Go model, whose
//! `zClient` is injected and whose query command is dispatched by the app.
//!
//! The modal draws into a fixed rect: the whole rect is filled with the modal
//! background first, then the border and content are drawn inside it — nothing
//! is ever drawn outside the rect. Content that does not fit in the content
//! area is dropped line by line, mirroring the Go `Renderer`'s silent capacity
//! behavior (see [`SectionSim`]).
//!
//! Shared helpers ([`key_matches_hotkey`], [`SectionSim`],
//! [`add_section_drawn`], [`put_content_line`]) live in
//! [`crate::prompt`] and are reused here.
//!
//! Accepted parity gaps vs. the Go version:
//! - the `quit` hotkey only closes the modal when zoxide is unavailable (Go
//!   has no `quit` case in the available branch, so `q` is typed into the
//!   input there);
//! - the stale-result guard is by `req_id` ([`Zoxide::set_results`]) rather
//!   than Go's query-string comparison; [`Zoxide::mark_queried`] records the
//!   request id the app dispatched and clears [`Zoxide::needs_query`];
//! - the border uses the plain Unicode border set (the draw() signature has no
//!   access to the config's custom border glyphs), and an unfocused border
//!   falls back to `pal.modal_fg` (the palette has no inactive modal border
//!   color);
//! - path truncation uses the stored modal width (`self.width - 13`) for its
//!   budget, exactly like Go's `Render`, while the draw rect is taken from the
//!   draw() parameters.

use ratatui::style::Color;
use crate::render::BorderSet;

use crate::config::{Hotkeys, Palette};
use crate::icons::Ui;
use crate::keys::{Key, KeyKind};
use crate::prompt::{add_section_drawn, key_matches_hotkey, put_content_line, SectionSim};
use crate::render::{RBuf, St};
use crate::text_input::{InputEvent, TextInput};
use crate::util::truncate_beginning;

// ---------------------------------------------------------------------------
// Constants (Go: zoxide/consts.go, render.go)
// ---------------------------------------------------------------------------

const ZOXIDE_HEADLINE: &str = "Zoxide Navigation";

const ZOXIDE_MIN_WIDTH: usize = 15;
const ZOXIDE_MIN_HEIGHT: usize = 3;

/// Maximum number of results visible at once (Go: maxVisibleResults).
const MAX_VISIBLE_RESULTS: usize = 5;

/// Width reserved for the score column: borders(2) + padding(2) + score(6) +
/// separator(3) (Go: scoreColumnWidth).
const SCORE_COLUMN_WIDTH: usize = 13;

/// Total padding for the modal input field: 2 (borders) + 1 (space) + 2
/// (prompt) + 1 (extra view char) (Go: modalInputPadding).
const ZOXIDE_INPUT_PADDING: usize = 6;

const SCROLL_UP_INDICATOR: &str = " \u{2191} More results above";
const SCROLL_DOWN_INDICATOR: &str = " \u{2193} More results below";

const NOT_AVAILABLE_MSG: &str = " Zoxide not available (check zoxide_support in config)";
const NO_RESULTS_MSG: &str = " No zoxide results found";

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

/// What a key press in the zoxide modal caused the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoxideAction {
    /// Nothing to do (typing, navigation, swallowing the opening key).
    None,
    /// The modal was closed (cancel/quit, or a confirm with no selection).
    Close,
    /// Confirm with a valid selection: change to the selected result's path.
    Cd(String),
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/// The zoxide modal (Go: `ui/zoxide.Model`).
#[derive(Debug)]
pub struct Zoxide {
    /// Whether a zoxide client is available (Go: `zClient == nil`).
    available: bool,
    open: bool,
    /// Flag to ignore the opening keystroke (Go: justOpened).
    just_opened: bool,
    input: TextInput,
    /// Query results as (score, path) (Go: []zoxidelib.Result).
    results: Vec<(f64, String)>,
    /// Index of the currently selected result (Go: cursor).
    cursor: usize,
    /// Index of the first visible result in the scrollable list (Go: renderIndex).
    render_index: usize,
    /// Set when the input was edited and a query should be dispatched.
    needs_query: bool,
    /// The id of the most recent request the app dispatched
    /// (Go: reqCnt; results are matched against it).
    latest_req: u64,
    /// Modal dimensions (border included), from the last layout pass.
    width: usize,
    height: usize,
}

impl Zoxide {
    /// A closed zoxide modal. `available` mirrors Go's `zClient != nil`.
    pub fn new(available: bool) -> Self {
        let mut input = TextInput::new();
        input.set_width(ZOXIDE_MIN_WIDTH - ZOXIDE_INPUT_PADDING);
        Self {
            available,
            open: false,
            just_opened: false,
            input,
            results: Vec::new(),
            cursor: 0,
            render_index: 0,
            needs_query: false,
            latest_req: 0,
            width: ZOXIDE_MIN_WIDTH,
            height: ZOXIDE_MIN_HEIGHT,
        }
    }

    /// Open the modal (Go: `Open`). The opening keystroke is swallowed via
    /// `just_opened`.
    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.clear_input();
        self.results.clear();
        self.cursor = 0;
        self.render_index = 0;
        self.needs_query = false;
    }

    /// Close the modal (Go: `Close`).
    pub fn close(&mut self) {
        self.open = false;
        self.clear_input();
        self.results.clear();
        self.cursor = 0;
        self.render_index = 0;
        self.needs_query = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The raw query text (Go: `GetTextInputValue`).
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// True when the input was edited and the app should dispatch a zoxide
    /// query for the current [`value`](Self::value).
    pub fn needs_query(&self) -> bool {
        self.needs_query
    }

    /// Record that the app dispatched a query with `req_id`; results arriving
    /// with a different id are ignored (Go: `reqCnt`), and the pending-query
    /// flag is cleared.
    pub fn mark_queried(&mut self, req_id: u64) {
        self.latest_req = req_id;
        self.needs_query = false;
    }

    /// Apply an async query result (Go: `UpdateMsg.Apply`). Results whose
    /// `req_id` is not the most recently dispatched one are ignored as stale;
    /// a fresh result replaces the list and resets the selection and scroll.
    pub fn set_results(&mut self, req_id: u64, results: Vec<(f64, String)>) {
        if req_id != self.latest_req {
            return;
        }
        self.results = results;
        self.cursor = 0;
        self.render_index = 0;
    }

    /// The path of the currently selected result, if any.
    pub fn selected(&self) -> Option<&str> {
        self.results.get(self.cursor).map(|(_, path)| path.as_str())
    }

    /// Modal width (border included), after clamping.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Modal height (border included), after clamping.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Modal dimensions, border included (Go: `SetWidth`/`SetMaxHeight`).
    pub fn set_dimensions(&mut self, w: u16, h: u16) {
        self.width = (w as usize).max(ZOXIDE_MIN_WIDTH);
        self.height = (h as usize).max(ZOXIDE_MIN_HEIGHT);
        self.input.set_width(self.width.saturating_sub(ZOXIDE_INPUT_PADDING));
    }

    fn clear_input(&mut self) {
        self.input.buf.clear();
        self.input.cursor = 0;
        self.input.scroll = 0;
    }

    /// Feed a key into the modal (Go: `HandleUpdate` for key presses).
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> ZoxideAction {
        if !self.open {
            return ZoxideAction::None;
        }

        // If zoxide is not available, only allow confirm/cancel/quit to close
        // the modal; nothing is typed.
        if !self.available {
            let closing = hk
                .confirm_typing
                .iter()
                .any(|h| key_matches_hotkey(h, k))
                || hk.cancel_typing.iter().any(|h| key_matches_hotkey(h, k))
                || hk.quit.iter().any(|h| key_matches_hotkey(h, k));
            if closing {
                self.close();
                return ZoxideAction::Close;
            }
            return ZoxideAction::None;
        }

        // Confirm always closes the modal afterwards (Go: handleConfirm then
        // Close).
        if hk.confirm_typing.iter().any(|h| key_matches_hotkey(h, k)) {
            let action = if self.cursor < self.results.len() {
                ZoxideAction::Cd(self.results[self.cursor].1.clone())
            } else {
                ZoxideAction::Close
            };
            self.close();
            return action;
        }
        if hk.cancel_typing.iter().any(|h| key_matches_hotkey(h, k)) {
            self.close();
            return ZoxideAction::Close;
        }
        // Alphanumeric list keys (like the default `j`/`k`) get typed into the
        // input rather than navigating, since the panel is in text-input mode
        // by default.
        if hk.list_up.iter().any(|h| key_matches_hotkey(h, k)) && !is_key_alnum(k) {
            self.navigate_up();
            return ZoxideAction::None;
        }
        if hk.list_down.iter().any(|h| key_matches_hotkey(h, k)) && !is_key_alnum(k) {
            self.navigate_down();
            return ZoxideAction::None;
        }
        // Ignore the key that just opened this modal so it doesn't appear in
        // the text input (Go: justOpened).
        if self.just_opened && hk.open_zoxide.iter().any(|h| key_matches_hotkey(h, k)) {
            self.just_opened = false;
            return ZoxideAction::None;
        }
        // Default: type into the input; only edits change the query.
        let ev = self.input.handle_key(k);
        if matches!(ev, InputEvent::Edited) {
            self.needs_query = true;
        }
        ZoxideAction::None
    }

    /// Go `navigateUp` (wraps to the bottom).
    fn navigate_up(&mut self) {
        if self.results.is_empty() {
            return;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
        } else {
            self.cursor = self.results.len() - 1;
        }
        self.update_render_index();
    }

    /// Go `navigateDown` (wraps to the top).
    fn navigate_down(&mut self) {
        if self.results.is_empty() {
            return;
        }
        if self.cursor < self.results.len() - 1 {
            self.cursor += 1;
        } else {
            self.cursor = 0;
        }
        self.update_render_index();
    }

    /// Go `updateRenderIndex`: keep the cursor within the visible window and
    /// clamp the window into range.
    fn update_render_index(&mut self) {
        if self.results.is_empty() {
            self.render_index = 0;
            return;
        }
        // If the cursor is above the visible range, scroll up.
        if self.cursor < self.render_index {
            self.render_index = self.cursor;
        }
        // If the cursor is below the visible range, scroll down.
        if self.cursor >= self.render_index + MAX_VISIBLE_RESULTS {
            self.render_index = self.cursor - MAX_VISIBLE_RESULTS + 1;
        }
        // Ensure render_index is within bounds.
        let max_render_index = self.results.len().saturating_sub(MAX_VISIBLE_RESULTS);
        if self.render_index > max_render_index {
            self.render_index = max_render_index;
        }
    }

    /// Render into `buf` at (x, y) with size (w, h) — the modal BOX (border
    /// included). The whole rect is filled with the modal background first;
    /// nothing is drawn outside it.
    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        focused: bool,
        pal: &Palette,
        ui: &Ui,
        hk: &Hotkeys,
    ) {
        let _ = hk; // Zoxide hints do not depend on hotkeys.

        // Components must cover their previous frame completely.
        buf.fill(x, y, w, h, St::fg_bg(pal.modal_fg, pal.modal_bg));
        if w < 2 || h < 2 {
            return;
        }

        let border_fg = if focused { pal.modal_border } else { pal.modal_fg };
        // Go: icon.Search + icon.Space + zoxideHeadlineText.
        let title = format!("{}{}{}", ui.search, ui.space, ZOXIDE_HEADLINE);
        buf.border_title(x, y, w, h, BorderSet::plain(), border_fg, pal.modal_bg, &title, border_fg);

        let cx = x + 1;
        let cw = w.saturating_sub(2);
        let ch = h.saturating_sub(2);
        if cw == 0 || ch == 0 {
            return;
        }

        let mut sim = SectionSim::new(ch);

        if !self.available {
            // Go: AddSection(); AddLines(" Zoxide not available ...") — no input line.
            add_section_drawn(&mut sim, buf, cx, y, cw, border_fg, pal);
            put_content_line(&mut sim, buf, cx, y, cw, NOT_AVAILABLE_MSG, pal.modal_fg, pal);
            return;
        }

        // Go: AddLines(" " + textInput.View()) — the zoxide input has no prompt.
        put_content_line(
            &mut sim,
            buf,
            cx,
            y,
            cw,
            &format!(" {}", self.input.view(focused)),
            pal.modal_fg,
            pal,
        );
        add_section_drawn(&mut sim, buf, cx, y, cw, border_fg, pal);

        if self.results.is_empty() {
            put_content_line(&mut sim, buf, cx, y, cw, NO_RESULTS_MSG, pal.modal_fg, pal);
            return;
        }

        self.render_result_list(&mut sim, buf, cx, y, cw, border_fg, pal);
    }

    /// Go `renderResultList` + `renderVisibleResults` + `renderScrollIndicators`.
    fn render_result_list(
        &self,
        sim: &mut SectionSim,
        buf: &mut RBuf,
        cx: usize,
        y: usize,
        cw: usize,
        border_fg: Color,
        pal: &Palette,
    ) {
        // Go's path-width budget uses the stored modal width.
        let available_path_width = self.width.saturating_sub(SCORE_COLUMN_WIDTH);

        let end_index = (self.render_index + MAX_VISIBLE_RESULTS).min(self.results.len());
        for i in self.render_index..end_index {
            let (score, path) = &self.results[i];
            let truncated = truncate_beginning(path, available_path_width, "...");
            // Go: fmt.Sprintf(" %6.1f | %s", score, path).
            let line = format!(" {:6.1} | {truncated}", score);
            let fg = if i == self.cursor { pal.cursor } else { pal.modal_fg };
            put_content_line(sim, buf, cx, y, cw, &line, fg, pal);
        }

        // Scroll indicators only when the list is longer than the window.
        if self.results.len() > MAX_VISIBLE_RESULTS {
            if self.render_index > 0 {
                add_section_drawn(sim, buf, cx, y, cw, border_fg, pal);
                put_content_line(sim, buf, cx, y, cw, SCROLL_UP_INDICATOR, pal.modal_fg, pal);
            }
            if end_index < self.results.len() {
                if self.render_index == 0 {
                    add_section_drawn(sim, buf, cx, y, cw, border_fg, pal);
                }
                put_content_line(sim, buf, cx, y, cw, SCROLL_DOWN_INDICATOR, pal.modal_fg, pal);
            }
        }
    }
}

/// Whether the key is a plain (no ctrl/alt) alphanumeric character
/// (Go: `isKeyAlphaNum`). Used to keep `j`/`k` out of navigation.
fn is_key_alnum(k: &Key) -> bool {
    if k.ctrl || k.alt {
        return false;
    }
    matches!(k.kind, KeyKind::Ch(c) if c.is_alphanumeric())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Theme;
    use crate::icons::ui_icons;
    use crate::keys;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> Key {
        keys::from_event(&KeyEvent::new(code, mods)).unwrap()
    }

    fn char_key(c: char) -> Key {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn enter() -> Key {
        key(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn results(n: usize) -> Vec<(f64, String)> {
        (0..n).map(|i| (i as f64 + 1.0, format!("/path/{}", i))).collect()
    }

    // -- navigation ------------------------------------------------------------

    #[test]
    fn navigate_wrap() {
        let mut z = Zoxide::new(true);
        z.open();
        z.mark_queried(1);
        z.set_results(1, results(2));
        // set_results resets cursor/render_index.
        assert_eq!(z.cursor, 0);
        assert_eq!(z.render_index, 0);

        z.navigate_down();
        assert_eq!(z.cursor, 1);
        // Wrap to top.
        z.navigate_down();
        assert_eq!(z.cursor, 0);
        // Wrap to bottom.
        z.navigate_up();
        assert_eq!(z.cursor, 1);
        z.navigate_up();
        assert_eq!(z.cursor, 0);
    }

    #[test]
    fn navigate_render_index_window() {
        let mut z = Zoxide::new(true);
        z.open();
        z.mark_queried(1);
        z.set_results(1, results(10));

        // Scroll down until the cursor is past the window.
        for _ in 0..6 {
            z.navigate_down();
        }
        assert_eq!(z.cursor, 6);
        // Window now starts at cursor - 4 (keeps 5 visible, cursor last).
        assert_eq!(z.render_index, 2);

        // Scroll back to the top: the window follows the cursor up.
        for _ in 0..6 {
            z.navigate_up();
        }
        assert_eq!(z.cursor, 0);
        assert_eq!(z.render_index, 0);
    }

    #[test]
    fn render_index_max_clamp() {
        let mut z = Zoxide::new(true);
        z.open();
        z.mark_queried(1);
        z.set_results(1, results(3));
        // With only 3 results, the window cannot start past index 0.
        z.cursor = 1;
        z.render_index = 1;
        z.update_render_index();
        assert_eq!(z.render_index, 0);
    }

    #[test]
    fn navigate_empty_results() {
        let mut z = Zoxide::new(true);
        z.open();
        z.navigate_up();
        z.navigate_down();
        assert_eq!(z.cursor, 0);
        assert_eq!(z.render_index, 0);
    }

    // -- request tracking ------------------------------------------------------

    #[test]
    fn stale_results_ignored() {
        let mut z = Zoxide::new(true);
        z.open();
        z.mark_queried(5);
        // An older request (id 3) arriving after the latest (5) is dropped.
        z.set_results(3, vec![(1.0, "old".to_string())]);
        assert!(z.results.is_empty());
        assert_eq!(z.selected(), None);
        // The latest request is applied.
        z.set_results(5, vec![(2.0, "new".to_string())]);
        assert_eq!(z.results.len(), 1);
        assert_eq!(z.selected(), Some("new"));
        assert_eq!(z.cursor, 0);
        assert_eq!(z.render_index, 0);
    }

    #[test]
    fn mark_queried_clears_needs_query() {
        let hk = Hotkeys::default();
        let mut z = Zoxide::new(true);
        z.open();
        assert!(!z.needs_query());
        assert_eq!(z.handle_key(&char_key('a'), &hk), ZoxideAction::None);
        assert!(z.needs_query());
        z.mark_queried(1);
        assert!(!z.needs_query());
        // Cursor movement does not re-flag a query.
        assert_eq!(z.handle_key(&key(KeyCode::Left, KeyModifiers::NONE), &hk), ZoxideAction::None);
        assert!(!z.needs_query());
    }

    // -- is_key_alnum ------------------------------------------------------------

    #[test]
    fn is_key_alnum_test() {
        assert!(is_key_alnum(&char_key('a')));
        assert!(is_key_alnum(&char_key('5')));
        assert!(is_key_alnum(&key(KeyCode::Char('Z'), KeyModifiers::SHIFT)));
        assert!(!is_key_alnum(&key(KeyCode::Up, KeyModifiers::NONE)));
        assert!(!is_key_alnum(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_key_alnum(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!is_key_alnum(&key(KeyCode::Char('a'), KeyModifiers::ALT)));
    }

    // -- unavailable ---------------------------------------------------------------

    #[test]
    fn unavailable_only_closing() {
        let hk = Hotkeys::default();
        let mut z = Zoxide::new(false);
        z.open();
        // quit / cancel / confirm all close.
        assert_eq!(z.handle_key(&char_key('q'), &hk), ZoxideAction::Close);
        assert!(!z.is_open());
        z.open();
        assert_eq!(z.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE), &hk), ZoxideAction::Close);
        assert!(!z.is_open());
        z.open();
        assert_eq!(z.handle_key(&enter(), &hk), ZoxideAction::Close);
        assert!(!z.is_open());
        // Any other key is ignored (not typed, not closed).
        z.open();
        assert_eq!(z.handle_key(&char_key('a'), &hk), ZoxideAction::None);
        assert!(z.is_open());
        assert_eq!(z.value(), "");
    }

    // -- opening key ----------------------------------------------------------------

    #[test]
    fn just_opened_swallow() {
        let hk = Hotkeys::default();
        let mut z = Zoxide::new(true);
        z.open();
        assert!(z.just_opened);
        // The 'z' that opened the modal is swallowed.
        assert_eq!(z.handle_key(&char_key('z'), &hk), ZoxideAction::None);
        assert!(!z.just_opened);
        assert_eq!(z.value(), "");
        // A subsequent 'z' is typed.
        assert_eq!(z.handle_key(&char_key('z'), &hk), ZoxideAction::None);
        assert_eq!(z.value(), "z");
    }

    // -- confirm --------------------------------------------------------------------

    #[test]
    fn confirm_cd_and_close() {
        let hk = Hotkeys::default();
        let mut z = Zoxide::new(true);
        z.open();
        z.mark_queried(1);
        z.set_results(1, vec![
            (1.0, "/tmp/a".to_string()),
            (2.0, "/tmp/b".to_string()),
        ]);
        // cursor 0 → Cd(first path), and the modal closes.
        assert_eq!(
            z.handle_key(&enter(), &hk),
            ZoxideAction::Cd("/tmp/a".to_string())
        );
        assert!(!z.is_open());
        // Navigate then confirm selects the moved cursor.
        z.open();
        z.mark_queried(2);
        z.set_results(2, vec![(1.0, "/tmp/a".to_string()), (2.0, "/tmp/b".to_string())]);
        z.navigate_down();
        assert_eq!(
            z.handle_key(&enter(), &hk),
            ZoxideAction::Cd("/tmp/b".to_string())
        );
        assert!(!z.is_open());
        // No results → confirm closes.
        z.open();
        assert_eq!(z.handle_key(&enter(), &hk), ZoxideAction::Close);
        assert!(!z.is_open());
    }

    // -- alnum list keys ---------------------------------------------------------------

    #[test]
    fn alnum_list_keys_type_not_navigate() {
        let hk = Hotkeys::default();
        let mut z = Zoxide::new(true);
        z.open();
        z.mark_queried(1);
        z.set_results(1, results(2));
        // 'k' is the default list_up but is alphanumeric → typed, not navigation.
        assert_eq!(z.handle_key(&char_key('k'), &hk), ZoxideAction::None);
        assert_eq!(z.value(), "k");
        assert_eq!(z.cursor, 0);
        // Arrow up navigates (wraps 0 → last with 2 results).
        assert_eq!(z.handle_key(&key(KeyCode::Up, KeyModifiers::NONE), &hk), ZoxideAction::None);
        assert_eq!(z.cursor, 1);
        // Arrow down navigates back.
        assert_eq!(z.handle_key(&key(KeyCode::Down, KeyModifiers::NONE), &hk), ZoxideAction::None);
        assert_eq!(z.cursor, 0);
    }

    // -- lifecycle -----------------------------------------------------------------------

    #[test]
    fn zoxide_lifecycle() {
        let mut z = Zoxide::new(true);
        assert!(!z.is_open());
        assert_eq!(z.width(), ZOXIDE_MIN_WIDTH);
        assert_eq!(z.height(), ZOXIDE_MIN_HEIGHT);
        z.set_dimensions(50, 15);
        assert_eq!(z.width(), 50);
        assert_eq!(z.height(), 15);
        // Clamped to the minimums (Go: SetWidth/SetMaxHeight warnings).
        z.set_dimensions(5, 1);
        assert_eq!(z.width(), ZOXIDE_MIN_WIDTH);
        assert_eq!(z.height(), ZOXIDE_MIN_HEIGHT);

        z.open();
        assert!(z.is_open());
        assert!(z.just_opened);
        z.mark_queried(1);
        z.set_results(1, results(3));
        assert_eq!(z.selected(), Some("/path/0"));
        z.close();
        assert!(!z.is_open());
        assert!(z.results.is_empty());
        assert_eq!(z.value(), "");
        assert_eq!(z.selected(), None);
        // Closed modals eat keys.
        let hk = Hotkeys::default();
        assert_eq!(z.handle_key(&char_key('a'), &hk), ZoxideAction::None);
        assert_eq!(z.value(), "");
    }

    // -- draw smoke ------------------------------------------------------------------------

    #[test]
    fn zoxide_draw_smoke() {
        let hk = Hotkeys::default();
        let pal = Theme::default().resolve();
        let ui = ui_icons(false);

        let mut z = Zoxide::new(true);
        z.set_dimensions(50, 15);
        z.open();
        z.mark_queried(1);
        z.set_results(1, vec![
            (100.0, "/tmp".to_string()),
            (50.0, "/home".to_string()),
        ]);
        let mut buf = RBuf::new(50, 15, Color::Black);
        z.draw(&mut buf, 0, 0, 50, 15, true, &pal, &ui, &hk);
        assert_eq!(buf.cell(0, 0).unwrap().ch, '\u{256d}');
        assert_eq!(buf.cell(49, 14).unwrap().ch, '\u{256f}');
        let top: String = (0..50).map(|x| buf.cell(x, 0).unwrap().ch).collect();
        assert!(top.contains("Zoxide Navigation"));

        // Unfocused border + a long list (scroll indicators).
        let mut z2 = Zoxide::new(true);
        z2.set_dimensions(50, 15);
        z2.open();
        z2.mark_queried(1);
        z2.set_results(1, results(8));
        let mut buf2 = RBuf::new(50, 15, Color::Black);
        z2.draw(&mut buf2, 0, 0, 50, 15, false, &pal, &ui, &hk);

        // Unavailable.
        let mut z3 = Zoxide::new(false);
        z3.set_dimensions(50, 15);
        z3.open();
        let mut buf3 = RBuf::new(50, 15, Color::Black);
        z3.draw(&mut buf3, 0, 0, 50, 15, true, &pal, &ui, &hk);
        // The message sits on content row 1 (below the section divider at
        // row 0), i.e. absolute row 2.
        let row: String = (0..50).map(|x| buf3.cell(x, 2).unwrap().ch).collect();
        assert!(row.contains("Zoxide not available"));

        // Tiny rects: no panics.
        let mut tiny = RBuf::new(3, 2, Color::Black);
        z.draw(&mut tiny, 0, 0, 3, 2, true, &pal, &ui, &hk);
        let mut tiny2 = RBuf::new(1, 1, Color::Black);
        z.draw(&mut tiny2, 0, 0, 1, 1, true, &pal, &ui, &hk);
    }
}
