//! Prompt modal: shell command line (`:`) and SPF command prompt (`>`).
//!
//! Rust port of the Go version's `src/internal/ui/prompt` package
//! (`model.go`, `consts.go`, `utils.go`, `tokenize.go`, `error.go`) together
//! with the section/capacity semantics of
//! `src/internal/ui/rendering/renderer_core.go` (see [SectionSim]).
//!
//! The modal draws into a fixed rect: the whole rect is filled with the
//! modal background first, then the border and the content are drawn inside
//! it — nothing is ever drawn outside the rect. Content that does not fit in
//! the content area (inner height) is dropped line by line, mirroring the Go
//! `Renderer`'s silent capacity behavior.
//!
//! Accepted parity gaps vs. the Go version:
//! - the `quit` hotkey closes the prompt (Go would type it into the input);
//! - tokenization errors are shown raw: the "Failed during tokenization : "
//!   prefix is not added here, wrapping is the app's job;
//! - `$()` substitutions run relative to the stored `cwd` (Go passes the
//!   panel's cwd into `HandleUpdate`); set it with [`Prompt::set_cwd`];
//! - substitution output is not trimmed (Go keeps trailing newlines);
//! - `close()` clears the result message (Go keeps it);
//! - the border uses the plain Unicode border set (the draw() signature has
//!   no access to the config's custom border glyphs), and an unfocused border
//!   falls back to `pal.modal_fg` (the palette has no inactive modal border
//!   color);
//! - the Go text input's `CharLimit` of 156 is not ported (the Rust
//!   [TextInput] has no character limit).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::style::Color;
use crate::render::BorderSet;

use crate::config::{Hotkeys, Palette};
use crate::icons::Ui;
use crate::keys::{self, Key};
use crate::render::{RBuf, St};
use crate::text_input::TextInput;

// ---------------------------------------------------------------------------
// Constants (Go: prompt/consts.go)
// ---------------------------------------------------------------------------

const PROMPT_HEADLINE: &str = "superfile Prompt";

pub const OPEN_COMMAND: &str = "open";
pub const SPLIT_COMMAND: &str = "split";
pub const CD_COMMAND: &str = "cd";

const SPF_PROMPT_CHAR: &str = ">";
const SHELL_PROMPT_CHAR: &str = ":";

const SUCCESS_PREFIX: &str = "Success";
const FAILURE_PREFIX: &str = "Error";

const SHELL_MODE_STRING: &str = "(Shell Mode)";
const SPF_MODE_STRING: &str = "(SPF Mode)";

const SPLIT_COMMAND_ARG_ERROR: &str = "split command should not be given arguments";

/// Timeout for commands executed for shell substitution (Go: shellSubTimeout).
const SHELL_SUB_TIMEOUT: Duration = Duration::from_millis(1000);

const PROMPT_MIN_WIDTH: usize = 10;
const PROMPT_MIN_HEIGHT: usize = 3;

/// Total padding for the prompt input field:
/// 2 (borders) + 1 (space) + 2 (prompt) + 1 (extra view char).
const PROMPT_INPUT_PADDING: usize = 6;

const EXPECTED_ARG_COUNT: usize = 2;

/// The SPF command hints, in display order (Go: defaultCommandSlice).
/// (command word, usage, description).
const SPF_COMMANDS: &[(&str, &str, &str)] = &[
    (OPEN_COMMAND, "open <PATH>", "Open a new panel at a specified path"),
    (SPLIT_COMMAND, "split", "Open a new panel at the current file panel's path"),
    (CD_COMMAND, "cd <PATH>", "Change directory of current panel"),
];

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// What a key press in the prompt modal caused the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAction {
    /// Nothing to do (typing, mode switch, error displayed in place).
    None,
    /// The modal was closed (cancel/quit, or empty confirm with
    /// close-on-success).
    Close,
    /// Shell mode confirm: run the raw input line in a shell.
    RunShell(String),
    /// SPF mode confirm: the input line validated cleanly; the string is the
    /// RESOLVED line (`${VAR}` / `$(cmd)` substitutions already applied — they
    /// run exactly once, here). The app tokenizes and parses it with
    /// [`tokenize_with_quotes`] and [`parse_spf_line`].
    RunSpf(String),
}

/// A parsed SPF command line (Go: the CD/Open/Split `common.ModelAction`s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpfCmd {
    Open(PathBuf),
    Cd(PathBuf),
    Split,
}

// ---------------------------------------------------------------------------
// Shared hotkey / rendering helpers (also used by zoxide)
// ---------------------------------------------------------------------------

/// Whether `k` (a normalized crossterm key) matches a configured hotkey
/// string.
///
/// Single-char hotkeys match on the pressed char itself (`Key::raw_char`,
/// case-sensitive, shift ignored) — exactly mirroring Go's tea comparison
/// `hotkey == msg.String()`, where the string of a char key is the char that
/// was pressed (so `":"` matches despite crossterm reporting SHIFT, and
/// `"z"` does not match a shifted `Z`). Ctrl/alt-modified presses never
/// match a single-char hotkey.
///
/// Multi-char hotkeys (`"ctrl+c"`, `"esc"`, `"shift+left"`, ...) are parsed
/// with [`crate::keys::parse`] and compared field by field, ignoring
/// `raw_char` (parsed hotkeys never carry one, crossterm events always do).
pub(crate) fn key_matches_hotkey(hotkey: &str, k: &Key) -> bool {
    let hk = hotkey.trim();
    if let Some(hc) = hk.chars().next() {
        if hk.chars().count() == 1 {
            return !k.ctrl && !k.alt && k.raw_char == Some(hc);
        }
    }
    let Some(parsed) = keys::parse(hotkey) else {
        return false;
    };
    parsed.kind == k.kind && parsed.ctrl == k.ctrl && parsed.shift == k.shift && parsed.alt == k.alt
}

/// Simulation of the Go `rendering.Renderer`'s section capacity: the content
/// area is `content_h` rows; `AddSection` commits the current section's rows
/// plus one divider row and silently no-ops when there is no room; lines
/// beyond the current section's capacity are dropped.
pub(crate) struct SectionSim {
    content_h: usize,
    committed: usize,
    cur_lines: usize,
    section_cap: usize,
}

impl SectionSim {
    pub(crate) fn new(content_h: usize) -> Self {
        Self {
            content_h,
            committed: 0,
            cur_lines: 0,
            section_cap: content_h,
        }
    }

    /// Go `AddSection`: silently fails (returns false) when the divider row
    /// would not fit; otherwise commits the current section plus the divider.
    pub(crate) fn add_section(&mut self) -> bool {
        if self.content_h <= self.committed + self.cur_lines {
            return false;
        }
        self.committed += self.cur_lines + 1;
        self.cur_lines = 0;
        self.section_cap = self.content_h - self.committed;
        true
    }

    /// Content-relative row of the divider added by the last successful
    /// [`add_section`](Self::add_section).
    pub(crate) fn divider_row(&self) -> usize {
        self.committed.saturating_sub(1)
    }

    /// Go `AddLine...`: the content-relative row for the line, or `None` when
    /// the current section is full (the line is dropped).
    pub(crate) fn add_line(&mut self) -> Option<usize> {
        if self.cur_lines >= self.section_cap {
            return None;
        }
        let row = self.committed + self.cur_lines;
        self.cur_lines += 1;
        Some(row)
    }
}

/// Draw the divider for a section that [`SectionSim::add_section`] accepted.
pub(crate) fn add_section_drawn(
    sim: &mut SectionSim,
    buf: &mut RBuf,
    cx: usize,
    y: usize,
    cw: usize,
    border_fg: Color,
    pal: &Palette,
) {
    if sim.add_section() {
        buf.section_divider(cx, y + 1 + sim.divider_row(), cw, BorderSet::plain(), border_fg, pal.modal_bg);
    }
}

/// Add one content line at (cx, y+1+row); returns the row or `None` when the
/// section is full (Go drops the line silently).
pub(crate) fn put_content_line(
    sim: &mut SectionSim,
    buf: &mut RBuf,
    cx: usize,
    y: usize,
    cw: usize,
    line: &str,
    fg: Color,
    pal: &Palette,
) -> Option<usize> {
    let row = sim.add_line()?;
    buf.put_line(cx, y + 1 + row, cw, line, St::new().fg(fg).bg(pal.modal_bg));
    Some(row)
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/// The prompt modal (Go: `ui/prompt.Model`).
#[derive(Debug)]
pub struct Prompt {
    open: bool,
    shell_mode: bool,
    input: TextInput,
    result_msg: String,
    action_success: bool,
    close_on_success: bool,
    /// Directory for `$()` shell substitution (Go: the `cwdLocation` arg of
    /// `HandleUpdate`).
    cwd: PathBuf,
    /// Modal dimensions (border included), from the last layout pass.
    width: usize,
    height: usize,
}

impl Prompt {
    /// A closed prompt in shell mode (Go: `GenerateModel` with
    /// `shellMode` defaulted to true by `Open`).
    pub fn new() -> Self {
        let mut input = TextInput::new();
        input.set_width(PROMPT_MIN_WIDTH - PROMPT_INPUT_PADDING);
        Self {
            open: false,
            shell_mode: true,
            input,
            result_msg: String::new(),
            action_success: true,
            close_on_success: false,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            width: PROMPT_MIN_WIDTH,
            height: PROMPT_MIN_HEIGHT,
        }
    }

    /// Open the modal in the given mode (Go: `Open`).
    pub fn open(&mut self, shell_mode: bool) {
        self.open = true;
        self.shell_mode = shell_mode;
    }

    /// Close the modal: back to shell mode, input and result cleared.
    pub fn close(&mut self) {
        self.open = false;
        self.shell_mode = true;
        self.clear_input();
        self.result_msg.clear();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn shell_mode(&self) -> bool {
        self.shell_mode
    }

    /// The raw input value (Go: `textInput.Value()`).
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// Directory relative to which `$()` shell substitutions run.
    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
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
        self.width = (w as usize).max(PROMPT_MIN_WIDTH);
        self.height = (h as usize).max(PROMPT_MIN_HEIGHT);
        self.input.set_width(self.width.saturating_sub(PROMPT_INPUT_PADDING));
    }

    /// Record the outcome of the action just performed
    /// (Go: `HandleShellCommandResults`/`HandleSPFActionResults` plus
    /// `CloseOnSuccessIfNeeded`).
    pub fn set_result(&mut self, success: bool, msg: &str, close_on_success: bool) {
        self.action_success = success;
        self.result_msg = msg.to_string();
        self.close_on_success = close_on_success;
        if success && close_on_success {
            self.close();
        }
    }

    fn clear_input(&mut self) {
        self.input.buf.clear();
        self.input.cursor = 0;
        self.input.scroll = 0;
    }

    /// Feed a key into the modal (Go: `HandleUpdate` for key presses).
    pub fn handle_key(&mut self, k: &Key, hk: &Hotkeys) -> PromptAction {
        if !self.open {
            return PromptAction::None;
        }
        if hk.confirm_typing.iter().any(|h| key_matches_hotkey(h, k)) {
            return self.handle_confirm();
        }
        if hk.cancel_typing.iter().any(|h| key_matches_hotkey(h, k)) {
            self.close();
            return PromptAction::Close;
        }
        if hk.quit.iter().any(|h| key_matches_hotkey(h, k)) {
            self.close();
            return PromptAction::Close;
        }
        // Mode switch only while the input is empty
        // (Go: handleNormalKeyInput's first two cases).
        if self.value().is_empty() {
            if let Some(h) = hk.open_spf_prompt.first() {
                if key_matches_hotkey(h, k) {
                    self.shell_mode = false;
                    self.result_msg.clear();
                    self.action_success = true;
                    return PromptAction::None;
                }
            }
            if let Some(h) = hk.open_command_line.first() {
                if key_matches_hotkey(h, k) {
                    self.shell_mode = true;
                    self.result_msg.clear();
                    self.action_success = true;
                    return PromptAction::None;
                }
            }
        }
        self.input.handle_key(k);
        self.result_msg.clear();
        self.action_success = true;
        PromptAction::None
    }

    /// Go `handleConfirm`, 1:1 (incl. the input-clearing order).
    fn handle_confirm(&mut self) -> PromptAction {
        // Pressing confirm on an empty prompt triggers close-on-success, then
        // getPromptAction("") → (NoAction, nil), which resets msg/flag.
        if self.value().is_empty() {
            if self.close_on_success && self.action_success {
                self.close();
                return PromptAction::Close;
            }
            self.result_msg.clear();
            self.action_success = true;
            return PromptAction::None;
        }

        let value = self.input.value().to_string();

        let outcome: Result<PromptAction, String> = if self.shell_mode {
            Ok(PromptAction::RunShell(value))
        } else {
            match resolve_substitutions(&value, &self.cwd) {
                Ok(resolved) => match tokenize_with_quotes(&resolved) {
                    Ok(tokens) => match parse_spf_line(&tokens) {
                        Ok(_) => Ok(PromptAction::RunSpf(resolved)),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            }
        };

        match outcome {
            Ok(action) => {
                self.result_msg.clear();
                self.action_success = true;
                self.clear_input();
                action
            }
            Err(e) => {
                self.result_msg = e;
                self.action_success = false;
                self.clear_input();
                PromptAction::None
            }
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
        // Components must cover their previous frame completely.
        buf.fill(x, y, w, h, St::fg_bg(pal.modal_fg, pal.modal_bg));
        if w < 2 || h < 2 {
            return;
        }

        let border_fg = if focused { pal.modal_border } else { pal.modal_fg };
        let title = format!(
            "{icon}{space}{headline} {mode}",
            icon = ui.terminal,
            space = ui.space,
            headline = PROMPT_HEADLINE,
            mode = if self.shell_mode { SHELL_MODE_STRING } else { SPF_MODE_STRING },
        );
        buf.border_title(x, y, w, h, BorderSet::plain(), border_fg, pal.modal_bg, &title, border_fg);

        let cx = x + 1;
        let cw = w.saturating_sub(2);
        let ch = h.saturating_sub(2);
        if cw == 0 || ch == 0 {
            return;
        }

        let mut sim = SectionSim::new(ch);

        // Go: `r.AddLines(" " + m.textInput.View())` — the Go input's View()
        // includes its ": "/" > " prompt, which the Rust input does not.
        let prompt_char = if self.shell_mode { SHELL_PROMPT_CHAR } else { SPF_PROMPT_CHAR };
        put_content_line(
            &mut sim,
            buf,
            cx,
            y,
            cw,
            &format!(" {prompt_char} {}", self.input.view(focused)),
            pal.modal_fg,
            pal,
        );

        if !self.shell_mode {
            // To make sure the hint section is added one time only per render
            // call (Go: hintSectionAdded).
            let mut hint_section_added = false;
            if self.value().is_empty() {
                if !hint_section_added {
                    add_section_drawn(&mut sim, buf, cx, y, cw, border_fg, pal);
                    hint_section_added = true;
                }
                let hk_label = hk.open_command_line.first().map(String::as_str).unwrap_or("");
                put_content_line(
                    &mut sim,
                    buf,
                    cx,
                    y,
                    cw,
                    &format!(" '{hk_label}' - Get into Shell mode"),
                    pal.modal_fg,
                    pal,
                );
            }
            // Go: strings.HasPrefix(cmd.command, getFirstToken(value))
            let first = first_token(self.value());
            for (command, usage, description) in SPF_COMMANDS {
                if command.starts_with(first) {
                    if !hint_section_added {
                        add_section_drawn(&mut sim, buf, cx, y, cw, border_fg, pal);
                        hint_section_added = true;
                    }
                    put_content_line(
                        &mut sim,
                        buf,
                        cx,
                        y,
                        cw,
                        &format!(" '{usage}' - {description}"),
                        pal.modal_fg,
                        pal,
                    );
                }
            }
        } else if self.value().is_empty() {
            let hk_label = hk.open_spf_prompt.first().map(String::as_str).unwrap_or("");
            add_section_drawn(&mut sim, buf, cx, y, cw, border_fg, pal);
            put_content_line(&mut sim, buf, cx, y, cw, &format!(" '{hk_label}' - Get into SPF mode"), pal.modal_fg, pal);
        }

        if !self.result_msg.is_empty() {
            let (prefix, fg) = if self.action_success {
                (SUCCESS_PREFIX, pal.correct)
            } else {
                (FAILURE_PREFIX, pal.error)
            };
            add_section_drawn(&mut sim, buf, cx, y, cw, border_fg, pal);
            // Go's AddLines splits on '\n'; each physical line consumes one
            // row of section capacity (the prefix stays on the first line).
            for physical in format!(" {prefix} : {}", self.result_msg).split('\n') {
                if put_content_line(&mut sim, buf, cx, y, cw, physical, fg, pal).is_none() {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SPF command parsing
// ---------------------------------------------------------------------------

/// First whitespace (space) token of a command, after trimming
/// (Go: `getFirstToken`).
fn first_token(command: &str) -> &str {
    let command = command.trim();
    match command.find(' ') {
        Some(i) => &command[..i],
        None => command,
    }
}

/// Parse a tokenized SPF command line (Go: `getPromptAction`'s switch).
///
/// An empty slice is an error here (Go would panic indexing `promptArgs[0]`).
pub fn parse_spf_line(tokens: &[String]) -> Result<SpfCmd, String> {
    let first = tokens.first().ok_or_else(|| "Invalid spf command : ".to_string())?;
    match first.as_str() {
        SPLIT_COMMAND => {
            if tokens.len() != 1 {
                return Err(SPLIT_COMMAND_ARG_ERROR.to_string());
            }
            Ok(SpfCmd::Split)
        }
        CD_COMMAND | OPEN_COMMAND => {
            if tokens.len() != EXPECTED_ARG_COUNT {
                return Err(format!("{first} command needs exactly one argument, received {}", tokens.len() - 1));
            }
            let path = PathBuf::from(&tokens[1]);
            if first == CD_COMMAND {
                Ok(SpfCmd::Cd(path))
            } else {
                Ok(SpfCmd::Open(path))
            }
        }
        _ => Err(format!("Invalid spf command : {first}")),
    }
}

// ---------------------------------------------------------------------------
// Shell substitution + tokenization (Go: prompt/tokenize.go, error.go)
// ---------------------------------------------------------------------------

/// Replace `${VAR}` (env lookup) and `$(command)` (shell exec) in `line`
/// with their values (Go: `resolveShellSubstitution`, 1:1 port, including the
/// error strings).
pub fn resolve_substitutions(line: &str, cwd: &Path) -> Result<String, String> {
    let runes: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < runes.len() {
        if i + 1 < runes.len() && runes[i] == '$' {
            let open_char = runes[i + 1];
            if !matches!(open_char, '(' | '{') {
                out.push(runes[i]);
                i += 1;
                continue;
            }
            let close_char = if open_char == '(' { ')' } else { '}' };
            let end = match find_ending_bracket(&runes, i + 1, open_char, close_char) {
                Some(e) => e,
                None => return Err("unexpected error in tokenization".to_string()),
            };
            if end == runes.len() {
                return Err(format!("could not find matching {close_char} for {open_char}"));
            }
            let token: String = runes[i + 2..end].iter().collect();
            match open_char {
                '{' => match std::env::var(&token) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => return Err(format!("env var {token} not found")),
                },
                '(' => match run_substitution_command(&token, cwd) {
                    Ok(output) => out.push_str(&output),
                    Err(e) => return Err(format!("could not execute shell substitution command : {token} : {e}")),
                },
                _ => return Err(format!("unexpected openChar {open_char:?} in tokenization")),
            }
            i = end + 1;
        } else {
            out.push(runes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Go `findEndingBracket`: index of the matching closing bracket, or
/// `Some(runes.len())` when there is no match; `None` when `runes[open_idx]`
/// is not the open bracket (Go returns -1 there, mapped to
/// "unexpected error in tokenization" by the caller).
fn find_ending_bracket(runes: &[char], open_idx: usize, open_paran: char, close_paran: char) -> Option<usize> {
    if open_idx >= runes.len() || runes[open_idx] != open_paran {
        return None;
    }
    let mut open_count: i32 = 1;
    let mut i = open_idx + 1;
    while i < runes.len() && open_count != 0 {
        match runes[i] {
            c if c == open_paran => open_count += 1,
            c if c == close_paran => open_count -= 1,
            _ => {}
        }
        if open_count != 0 {
            i += 1;
        }
    }
    Some(i)
}

/// Run `token` as `sh -c` in `cwd` with combined stdout+stderr captured and a
/// 1-second deadline (Go: `utils.ExecuteCommandInShell`). A non-zero exit
/// status is NOT an error — the output is still used (Go: same).
///
/// Error strings are Go-exact: `"context deadline exceeded"` on timeout,
/// `"unexpected Error in command execution : ..."` (capital E) on
/// spawn/wait failures.
fn run_substitution_command(token: &str, cwd: &Path) -> Result<String, String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(token)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("unexpected Error in command execution : {e}"))?;

    let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        let _ = tx_out.send(buf);
    });
    let (tx_err, rx_err) = mpsc::channel::<Vec<u8>>();
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        let _ = tx_err.send(buf);
    });

    let deadline = Instant::now() + SHELL_SUB_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("context deadline exceeded".to_string());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("unexpected Error in command execution : {e}"));
            }
        }
    }

    let out = rx_out.recv().unwrap_or_default();
    let err_out = rx_err.recv().unwrap_or_default();
    let mut combined = String::from_utf8_lossy(&out).to_string();
    let err_text = String::from_utf8_lossy(&err_out);
    if !err_text.is_empty() {
        combined.push_str(&err_text);
    }
    Ok(combined)
}

/// Split `command` into tokens respecting quotes and escapes
/// (Go: `tokenizeWithQuotes`, 1:1 port).
pub fn tokenize_with_quotes(command: &str) -> Result<Vec<String>, String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut buffer = String::new();
    let mut quote_open: Option<char> = None;
    let mut escaped = false;

    for r in command.chars() {
        if escaped {
            // Only these characters may be escaped; anything else keeps the
            // backslash literal.
            match r {
                '"' | '\'' | '\\' | ' ' => buffer.push(r),
                _ => {
                    buffer.push('\\');
                    buffer.push(r);
                }
            }
            escaped = false;
        } else if r == '\\' {
            escaped = true;
        } else if quote_open.is_none() && (r == '"' || r == '\'') {
            quote_open = Some(r);
        } else if Some(r) == quote_open {
            // End of quoted section — always flush (even if empty).
            tokens.push(std::mem::take(&mut buffer));
            quote_open = None;
        } else if r.is_whitespace() && quote_open.is_none() {
            // Only flush if we have content.
            if !buffer.is_empty() {
                tokens.push(std::mem::take(&mut buffer));
            }
        } else {
            buffer.push(r);
        }
    }

    if escaped || quote_open.is_some() {
        return Err("unmatched quotes or escape characters in command".to_string());
    }
    if !buffer.is_empty() {
        tokens.push(buffer);
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> Key {
        keys::from_event(&KeyEvent::new(code, mods)).unwrap()
    }

    fn char_key(c: char) -> Key {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    // -- tokenize_with_quotes ------------------------------------------------

    #[test]
    fn tokenize_basic() {
        assert_eq!(
            tokenize_with_quotes("open /tmp/foo").unwrap(),
            vec!["open".to_string(), "/tmp/foo".to_string()]
        );
        assert_eq!(tokenize_with_quotes("").unwrap(), Vec::<String>::new());
        assert_eq!(
            tokenize_with_quotes("a  b").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn tokenize_quotes_and_escapes() {
        assert_eq!(
            tokenize_with_quotes(r#"open "/tmp/a b""#).unwrap(),
            vec!["open".to_string(), "/tmp/a b".to_string()]
        );
        // Quotes always flush on close, even when empty.
        assert_eq!(
            tokenize_with_quotes(r#"open ''"#).unwrap(),
            vec!["open".to_string(), String::new()]
        );
        assert_eq!(
            tokenize_with_quotes("open a\\ b").unwrap(),
            vec!["open".to_string(), "a b".to_string()]
        );
        // Invalid escape: backslash kept literal.
        assert_eq!(
            tokenize_with_quotes("open a\\c").unwrap(),
            vec!["open".to_string(), "a\\c".to_string()]
        );
        assert_eq!(
            tokenize_with_quotes(r#"open "a'b""#).unwrap(),
            vec!["open".to_string(), "a'b".to_string()]
        );
    }

    #[test]
    fn tokenize_errors() {
        let err = "unmatched quotes or escape characters in command";
        assert_eq!(tokenize_with_quotes(r#"open "abc"#).unwrap_err(), err);
        assert_eq!(tokenize_with_quotes("open 'a").unwrap_err(), err);
        assert_eq!(tokenize_with_quotes("open a\\").unwrap_err(), err);
    }

    // -- parse_spf_line ------------------------------------------------------

    #[test]
    fn parse_spf_line_test() {
        let t = |s: &str| s.to_string();
        assert_eq!(parse_spf_line(&[t("split")]), Ok(SpfCmd::Split));
        assert_eq!(
            parse_spf_line(&[t("split"), t("x")]).unwrap_err(),
            "split command should not be given arguments"
        );
        assert_eq!(parse_spf_line(&[t("cd"), t("/tmp")]), Ok(SpfCmd::Cd(PathBuf::from("/tmp"))));
        assert_eq!(
            parse_spf_line(&[t("cd")]).unwrap_err(),
            "cd command needs exactly one argument, received 0"
        );
        assert_eq!(
            parse_spf_line(&[t("cd"), t("a"), t("b")]).unwrap_err(),
            "cd command needs exactly one argument, received 2"
        );
        assert_eq!(parse_spf_line(&[t("open"), t("/x")]), Ok(SpfCmd::Open(PathBuf::from("/x"))));
        assert_eq!(
            parse_spf_line(&[t("open")]).unwrap_err(),
            "open command needs exactly one argument, received 0"
        );
        assert_eq!(parse_spf_line(&[t("ls")]).unwrap_err(), "Invalid spf command : ls");
        // Go would panic here; the port returns an error.
        assert_eq!(parse_spf_line(&[]).unwrap_err(), "Invalid spf command : ");
    }

    // -- resolve_substitutions -----------------------------------------------

    #[test]
    fn substitution_plain_and_bare_dollar() {
        let cwd = std::env::temp_dir();
        assert_eq!(resolve_substitutions("ls -la", &cwd).unwrap(), "ls -la");
        assert_eq!(resolve_substitutions("cost is $5", &cwd).unwrap(), "cost is $5");
        assert_eq!(resolve_substitutions("a$b", &cwd).unwrap(), "a$b");
        // '$' as the last rune is written as-is.
        assert_eq!(resolve_substitutions("trailing $", &cwd).unwrap(), "trailing $");
    }

    #[test]
    fn substitution_env_var() {
        let cwd = std::env::temp_dir();
        std::env::set_var("SPF_RUST_TEST_VAR_9F3K", "hello world");
        assert_eq!(
            resolve_substitutions("cat ${SPF_RUST_TEST_VAR_9F3K}!", &cwd).unwrap(),
            "cat hello world!"
        );
        std::env::remove_var("SPF_RUST_TEST_VAR_9F3K");
        assert_eq!(
            resolve_substitutions("${SPF_RUST_TEST_VAR_MISSING_7Q2X}", &cwd).unwrap_err(),
            "env var SPF_RUST_TEST_VAR_MISSING_7Q2X not found"
        );
    }

    #[test]
    fn substitution_bracket_errors() {
        let cwd = std::env::temp_dir();
        assert_eq!(
            resolve_substitutions("echo ${unclosed", &cwd).unwrap_err(),
            "could not find matching } for {"
        );
        assert_eq!(
            resolve_substitutions("echo $(unclosed", &cwd).unwrap_err(),
            "could not find matching ) for ("
        );
    }

    #[test]
    fn substitution_command() {
        let cwd = std::env::temp_dir();
        // Go does not trim command output: the trailing newline survives.
        assert_eq!(resolve_substitutions("[$(echo hi)]", &cwd).unwrap(), "[hi\n]");
    }

    #[test]
    fn substitution_command_timeout() {
        let cwd = std::env::temp_dir();
        let err = resolve_substitutions("$(sleep 2)", &cwd).unwrap_err();
        // The token in the error is the INNER command (between the parens),
        // not the `$(...)` wrapper.
        assert_eq!(
            err,
            "could not execute shell substitution command : sleep 2 : context deadline exceeded"
        );
    }

    // -- key_matches_hotkey ----------------------------------------------------

    #[test]
    fn hotkey_matching() {
        // Single-char hotkeys match the pressed char verbatim; crossterm
        // reports ':' with SHIFT, Go still matches on the char.
        assert!(key_matches_hotkey(":", &key(KeyCode::Char(':'), KeyModifiers::SHIFT)));
        assert!(!key_matches_hotkey(":", &key(KeyCode::Char(';'), KeyModifiers::NONE)));
        assert!(key_matches_hotkey("z", &char_key('z')));
        assert!(!key_matches_hotkey("z", &key(KeyCode::Char('Z'), KeyModifiers::SHIFT)));
        assert!(key_matches_hotkey("Z", &key(KeyCode::Char('Z'), KeyModifiers::SHIFT)));
        // Ctrl/alt-modified presses never match a single-char hotkey.
        assert!(!key_matches_hotkey("q", &key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        // Multi-char hotkeys (raw_char ignored in the comparison).
        assert!(key_matches_hotkey("ctrl+c", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!key_matches_hotkey("ctrl+d", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(key_matches_hotkey("esc", &key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(key_matches_hotkey("enter", &key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(key_matches_hotkey("shift+left", &key(KeyCode::Left, KeyModifiers::SHIFT)));
        assert!(!key_matches_hotkey("", &char_key('a')));
        assert!(!key_matches_hotkey("notakey", &char_key('a')));
    }

    // -- first_token -----------------------------------------------------------

    #[test]
    fn first_token_test() {
        assert_eq!(first_token("open /tmp"), "open");
        assert_eq!(first_token("  open   /tmp"), "open");
        assert_eq!(first_token("open"), "open");
        assert_eq!(first_token(""), "");
    }

    // -- SectionSim -------------------------------------------------------------

    #[test]
    fn section_sim_capacity() {
        let mut s = SectionSim::new(4);
        assert_eq!(s.add_line(), Some(0));
        assert_eq!(s.add_line(), Some(1));
        assert!(s.add_section());
        assert_eq!(s.divider_row(), 2);
        assert_eq!(s.add_line(), Some(3));
        // section cap = 4 - 3 = 1
        assert_eq!(s.add_line(), None);
        // No room for another section: 4 <= 3 + 1.
        assert!(!s.add_section());
    }

    #[test]
    fn section_sim_first_section_empty() {
        let mut s = SectionSim::new(3);
        assert!(s.add_section());
        assert_eq!(s.divider_row(), 0);
        assert_eq!(s.add_line(), Some(1));
        assert_eq!(s.add_line(), Some(2));
        assert_eq!(s.add_line(), None);
    }

    // -- Prompt behavior ---------------------------------------------------------

    #[test]
    fn prompt_defaults_and_lifecycle() {
        let p = Prompt::new();
        assert!(!p.is_open());
        assert!(p.shell_mode());
        assert_eq!(p.value(), "");
        let mut p = p;
        p.set_dimensions(60, 20);
        assert_eq!(p.width(), 60);
        assert_eq!(p.height(), 20);
        // Clamped to the minimums (Go: SetWidth/SetMaxHeight warnings).
        p.set_dimensions(4, 1);
        assert_eq!(p.width(), PROMPT_MIN_WIDTH);
        assert_eq!(p.height(), PROMPT_MIN_HEIGHT);
        p.open(false);
        assert!(p.is_open());
        assert!(!p.shell_mode());
        p.close();
        assert!(!p.is_open());
        assert!(p.shell_mode());
        assert_eq!(p.value(), "");
    }

    #[test]
    fn prompt_confirm_shell() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(true);
        for c in "ls -la".chars() {
            assert_eq!(p.handle_key(&char_key(c), &hk), PromptAction::None);
        }
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk),
            PromptAction::RunShell("ls -la".to_string())
        );
        assert_eq!(p.value(), "");
    }

    #[test]
    fn prompt_confirm_spf() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(false);
        for c in "cd /tmp".chars() {
            assert_eq!(p.handle_key(&char_key(c), &hk), PromptAction::None);
        }
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk),
            PromptAction::RunSpf("cd /tmp".to_string())
        );
        assert_eq!(p.value(), "");
    }

    #[test]
    fn prompt_confirm_spf_errors() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(false);
        for c in "cd".chars() {
            p.handle_key(&char_key(c), &hk);
        }
        assert_eq!(p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk), PromptAction::None);
        assert!(!p.action_success);
        assert_eq!(p.result_msg, "cd command needs exactly one argument, received 0");
        assert_eq!(p.value(), "");

        let mut p = Prompt::new();
        p.open(false);
        for c in "ls /tmp".chars() {
            p.handle_key(&char_key(c), &hk);
        }
        assert_eq!(p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk), PromptAction::None);
        assert_eq!(p.result_msg, "Invalid spf command : ls");
    }

    #[test]
    fn prompt_confirm_empty() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(true);
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk),
            PromptAction::None
        );
        assert!(p.is_open());
    }

    #[test]
    fn prompt_close_on_success() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(true);
        // Go: CloseOnSuccessIfNeeded runs when the results arrive.
        p.set_result(true, "done", true);
        assert!(!p.is_open());
        p.open(true);
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk),
            PromptAction::Close
        );
        assert!(!p.is_open());
        // A failed result does not close.
        p.open(true);
        p.set_result(false, "boom", true);
        assert!(p.is_open());
    }

    #[test]
    fn prompt_mode_switch() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(true);
        // ':' while empty stays in shell mode.
        assert_eq!(p.handle_key(&key(KeyCode::Char(':'), KeyModifiers::SHIFT), &hk), PromptAction::None);
        assert!(p.shell_mode());
        // '>' switches to SPF mode.
        assert_eq!(p.handle_key(&key(KeyCode::Char('>'), KeyModifiers::SHIFT), &hk), PromptAction::None);
        assert!(!p.shell_mode());
        // Once the input has text, the mode-switch keys type into it.
        assert_eq!(p.handle_key(&char_key('a'), &hk), PromptAction::None);
        assert_eq!(p.value(), "a");
        assert_eq!(p.handle_key(&key(KeyCode::Char('>'), KeyModifiers::SHIFT), &hk), PromptAction::None);
        assert_eq!(p.value(), "a>");
        assert!(!p.shell_mode());
    }

    #[test]
    fn prompt_quit_and_cancel_close() {
        let hk = Hotkeys::default();
        let mut p = Prompt::new();
        p.open(true);
        // Spec-added: Go would type 'q' into the input.
        assert_eq!(p.handle_key(&char_key('q'), &hk), PromptAction::Close);
        assert!(!p.is_open());
        p.open(true);
        assert_eq!(p.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE), &hk), PromptAction::Close);
        assert!(!p.is_open());
        // Closed prompts eat keys.
        assert_eq!(p.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE), &hk), PromptAction::None);
    }

    // -- draw smoke tests ----------------------------------------------------------

    #[test]
    fn prompt_draw_smoke() {
        use crate::config::Theme;
        use crate::icons::ui_icons;

        let hk = Hotkeys::default();
        let pal = Theme::default().resolve();
        let ui = ui_icons(false);

        let mut p = Prompt::new();
        p.set_dimensions(40, 10);
        p.open(true);
        for c in "cd".chars() {
            p.handle_key(&char_key(c), &hk);
        }
        p.set_result(false, "cd: no such file\nsecond line", true);

        let mut buf = RBuf::new(40, 10, Color::Black);
        p.draw(&mut buf, 0, 0, 40, 10, true, &pal, &ui, &hk);
        assert_eq!(buf.cell(0, 0).unwrap().ch, '╭');
        assert_eq!(buf.cell(39, 9).unwrap().ch, '╯');
        // The title is embedded in the top border.
        let top: String = (0..40).map(|x| buf.cell(x, 0).unwrap().ch).collect();
        assert!(top.contains("superfile Prompt"));

        // Unfocused border.
        let mut buf2 = RBuf::new(40, 10, Color::Black);
        p.draw(&mut buf2, 0, 0, 40, 10, false, &pal, &ui, &hk);

        // Tiny rects: no panics.
        let mut buf3 = RBuf::new(3, 2, Color::Black);
        p.draw(&mut buf3, 0, 0, 3, 2, true, &pal, &ui, &hk);
        let mut buf4 = RBuf::new(1, 1, Color::Black);
        p.draw(&mut buf4, 0, 0, 1, 1, true, &pal, &ui, &hk);
    }
}
