//! Application core: state, terminal lifecycle, event loop, layout, rendering,
//! key dispatch, and worker orchestration (port of superfile v1.6.0 internal/model.go
//! + model_render.go + key_function.go + handle_*.go, app-level pieces).
//!
//! Deviations (documented):
//! 1. Poll-based event loop (crossterm poll ~16ms) instead of bubbletea's channel loop;
//!    tea.Cmd/tea.Msg become std::thread + mpsc.
//! 2. File-operation keys (paste/copy/cut/delete/create-confirm/zip/extract) and the
//!    spf-error skip/abort flow run as background workers with process-bar entries.
//! 3. Prompt shell mode runs in a worker thread (AsyncMsg::ShellDone) instead of Go's
//!    blocking-in-Update execution (identical observable behavior, no UI freeze).
//! 4. `$(cmd)` substitutions and the zoxide CLI query run in worker threads.
//! 5. zoxide availability = config.zoxide_support && the `zoxide` binary is on PATH;
//!    queries shell out to `zoxide query --interactive <value>` (output lines are
//!    "score\tpath") instead of the Go zoxide client library.
//! 6. The initial-path zoxide fallback for non-existent CLI paths is skipped
//!    (FilePanel::new falls back to $HOME, as does the Go error path).
//! 7. Metadata component is cache-less; stale results are dropped by the app via
//!    get_location/get_expected_focused (same observable result).
//! 8. exiftool handle cleanup / preview CleanUp are no-ops in Rust.
//! 9. firstLoadingComplete/"Loading..." frame is omitted (poll loop renders as soon
//!    as the first frame is ready; panels show their normal empty state).
//! 10. Editor open (tea.ExecProcess) is emulated by dropping raw mode + alt screen,
//!    running the command, then restoring — same observable behavior.
//! 11. MakePrintableWithEscCheck(output, false) → util::make_printable (esc passthrough;
//!     cosmetic difference on shell output containing ESC).
//! 12. read_bool_file treats unrecognized content (anything other than "true"/"false")
//!     as the default, per the Go ReadBoolFile contract ("any error/garbage → default").
//! 13. SPF-prompt `$(cmd)`/`${VAR}` substitutions are resolved exactly ONCE,
//!     inside Prompt::handle_key; RunSpf carries the resolved line and the app
//!     only tokenizes + parses it. AsyncMsg::SubstDone (never sent today) is
//!     still handled defensively in handle_async.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;

use crate::clipboard::Clipboard;
use crate::config::{Config, Hotkeys, Paths, Palette, Theme};
use crate::dialogs::{
    self, ConfirmAction, HelpMenu, Notify, SortMenu, SpfError, ThemeMenu, TypingModal, MODAL_H,
    MODAL_W, SORT_H, SORT_W, THEME_W,
};
use crate::event::AsyncMsg;
use crate::fileops::{self, OpKind, WorkerMsg};
use crate::icons::{self, Ui};
use crate::keys::{self, Key};
use crate::metadata;
use crate::panel::{PanelMode, SortKind};
use crate::panels::PanelGroup;
use crate::preview;
use crate::processbar::{Operation, ProcessBar, State};
use crate::prompt::{self, Prompt, PromptAction, SpfCmd};
use crate::render::{RBuf, St};
use crate::sidebar::Sidebar;
use crate::util;
use crate::zoxide::Zoxide;

const CURRENT_VERSION: &str = "v1.6.0"; // Go variable.CurrentVersion
const LATEST_VERSION_URL: &str = "https://api.github.com/repos/yorukot/superfile/releases/latest";
const LATEST_VERSION_GH: &str = "https://github.com/yorukot/superfile/releases";

// Go common/predefined_variable.go delete-warning strings.
const TRASH_WARN_TITLE: &str = "Are you sure you want to move this to trash can";
const TRASH_WARN_CONTENT: &str = "This operation will move file or directory to trash can.";
const PERM_DELETE_WARN_TITLE: &str = "Are you sure you want to completely delete";
const PERM_DELETE_WARN_CONTENT: &str = "This operation cannot be undone and your data will be completely lost.";

/// Which non-file-panel region is focused. File-panel focus = Focus::None
/// (the active panel is group.focused_idx()).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    None,
    ProcessBar,
    Sidebar,
    Metadata,
}

/// Go modelQuitState machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QuitState {
    NotQuitting,
    Initiated,
    ConfirmInitiated,
    ConfirmReceived,
    Done,
}

pub struct App {
    paths: Paths,
    config: Config,
    hotkeys: Hotkeys,
    theme: Theme,
    pal: Palette,
    ui: Ui,
    chooser_file: Option<PathBuf>,

    full_w: usize,
    full_h: usize,
    main_panel_h: usize, // excludes borders
    footer_h: usize,     // excludes borders
    toggle_footer: bool,
    display_dot_files: bool,
    first_use: bool,
    quit_state: QuitState,
    quit_cd_on_quit: bool,

    group: PanelGroup,
    sidebar: Sidebar,
    processbar: ProcessBar,
    clipboard_ui: Clipboard,
    metadata: metadata::Metadata,
    preview: preview::Preview,
    prompt: Prompt,
    zoxide: Zoxide,
    notify: Notify,
    spf_error: SpfError,
    sort_menu: SortMenu,
    theme_menu: ThemeMenu,
    help_menu: HelpMenu,
    typing_modal: TypingModal,
    focus: Focus,

    rx: mpsc::Receiver<AsyncMsg>,
    tx: mpsc::Sender<AsyncMsg>,
    md_rx: mpsc::Receiver<metadata::MetadataMsg>,
    md_tx: mpsc::Sender<metadata::MetadataMsg>,
    io_req: u64, // monotonically increasing id counter (Go ioReqCnt)
    ops: HashMap<u64, OpRecord>, // live background file operations (Go processbar processes)
    error_op: Option<u64>, // op whose failure opened the spf-error modal
    preview_spawn_dims: (usize, usize), // last (content w, content h) a preview was spawned with
    preview_closed_by_user: bool,
}

/// One live background file operation: what it is, where it writes, its
/// cancel flag (shared with the worker), its total item/file count, the
/// trash backend choice, and the error message stored by `OpFail` (reported
/// when the user aborts). These drive the skip/abort resume flow.
struct OpRecord {
    kind: OpKind,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    total: usize,
    use_trash: bool,
    err: String,
}

impl App {
    pub fn new(
        paths: Paths,
        first_panel_paths: Vec<PathBuf>,
        first_use: bool,
        chooser_file: Option<PathBuf>,
    ) -> Self {
        let config = Config::load(&paths.config_file_path()).unwrap_or_default();
        let hotkeys = Hotkeys::load(&paths.hotkey_file_path()).unwrap_or_default();
        let theme_dir = paths.theme_dir();
        let theme = Theme::load(&theme_dir, &config.theme).unwrap_or_default();
        let pal = theme.resolve();
        let ui = icons::ui_icons(config.nerdfont);
        let display_dot_files = read_bool_file(&paths.toggle_dot_file(), false);
        let toggle_footer = read_bool_file(&paths.toggle_footer_file(), true);
        let pinned_file = paths.pinned_file();

        // Initial paths: "" -> config.default_directory; relative -> joined to cwd.
        // (PanelGroup::new handles missing locations -> $HOME.)
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let first: Vec<PathBuf> = first_panel_paths
            .iter()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    PathBuf::from(&config.default_directory)
                } else if p.is_relative() {
                    cwd.join(p)
                } else {
                    p.clone()
                }
            })
            .collect();

        let (tx, rx) = mpsc::channel();
        let (md_tx, md_rx) = mpsc::channel();
        let zoxide_available = config.zoxide_support && which_bin("zoxide").is_some();

        // NOTE: struct-literal fields evaluate in written order; the fields that
        // borrow `config`/`hotkeys` must come before those values are moved in.
        App {
            group: PanelGroup::new(first, &config),
            sidebar: Sidebar::new(&config, pinned_file),
            preview: preview::Preview::new(config.enable_file_preview_border),
            help_menu: HelpMenu::new(&hotkeys),
            paths,
            config,
            hotkeys,
            theme,
            pal,
            ui,
            chooser_file,
            full_w: 0,
            full_h: 0,
            main_panel_h: 0,
            footer_h: 0,
            toggle_footer,
            display_dot_files,
            first_use,
            quit_state: QuitState::NotQuitting,
            quit_cd_on_quit: false,
            processbar: ProcessBar::new(),
            clipboard_ui: Clipboard::new(),
            metadata: metadata::Metadata::new(),
            prompt: Prompt::new(),
            zoxide: Zoxide::new(zoxide_available),
            notify: Notify::new(),
            spf_error: SpfError::new(),
            sort_menu: SortMenu::new(),
            theme_menu: ThemeMenu::new(),
            typing_modal: TypingModal::new(),
            focus: Focus::None,
            rx,
            tx,
            md_rx,
            md_tx,
            io_req: 0,
            ops: HashMap::new(),
            error_op: None,
            preview_spawn_dims: (0, 0),
            preview_closed_by_user: false,
        }
    }

    pub fn run(&mut self) -> Result<String, String> {
        terminal::enable_raw_mode().map_err(|e| e.to_string())?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide).map_err(|e| e.to_string())?;
        let (w, h) = terminal::size().map_err(|e| e.to_string())?;
        self.full_w = w as usize;
        self.full_h = h as usize;
        self.relayout();

        // Initial listing for all panels.
        let focus_idx = self.group.focused_idx();
        let focus_none = self.focus == Focus::None;
        let show = self.display_dot_files;
        let tx = self.tx.clone();
        for (i, panel) in self.group.panels_mut().iter_mut().enumerate() {
            if panel.needs_relist(focus_none && i == focus_idx) {
                self.io_req += 1;
                panel.spawn_list(self.io_req, show, tx.clone());
            }
        }

        let backend = CrosstermBackend::new(std::io::stdout());
        let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;
        loop {
            // 1. Drain async messages.
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_async(msg);
            }
            while let Ok(msg) = self.md_rx.try_recv() {
                self.handle_metadata(msg);
            }
            // 2. Poll terminal events (16ms).
            while event::poll(Duration::from_millis(16)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(k) => {
                        if let Some(key) = keys::from_event(&k) {
                            self.handle_key(&key);
                        }
                    }
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => self.wheel_up(),
                        MouseEventKind::ScrollDown => self.wheel_down(),
                        _ => {}
                    },
                    Event::Resize(w, h) => {
                        self.full_w = w as usize;
                        self.full_h = h as usize;
                        self.relayout();
                    }
                    _ => {}
                }
            }
            // 3. Post-message state (Go updateModelStateAfterMsg).
            self.sidebar.update_directories();
            self.relist_panels();
            if self.focus != Focus::Metadata {
                self.metadata.reset_render();
            }
            // 4. Dispatch zoxide queries + preview + metadata fetch.
            self.dispatch_zoxide_query();
            self.update_preview();
            self.update_metadata();
            // 5. Render.
            let mut frame = RBuf::new(self.full_w, self.full_h, self.pal.full_screen_bg);
            self.render(&mut frame);
            let mut tb =
                ratatui::buffer::Buffer::empty(Rect::new(0, 0, self.full_w as u16, self.full_h as u16));
            frame.blit(&mut tb);
            term.draw(|f| {
                f.buffer_mut().clone_from(&tb);
            })
            .map_err(|e| e.to_string())?;
            if self.quit_state == QuitState::ConfirmReceived {
                break;
            }
        }

        // Teardown.
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen, Show);

        // Go quitSuperfile.
        let last_dir = self.group.focused_panel().location.display().to_string();
        if self.quit_cd_on_quit {
            let escaped = last_dir.replace('\'', "'\\''");
            let _ = std::fs::write(self.paths.last_dir_file(), format!("cd '{escaped}'"));
        }
        self.quit_state = QuitState::Done;
        Ok(last_dir)
    }

    pub fn check_for_updates(&self) {
        if !self.config.auto_check_update {
            return;
        }
        let now = chrono::Utc::now();
        let last = read_last_check_time(&self.paths.last_check_version_file());
        // shouldCheckForUpdate: last is zero/unparseable OR now - last >= 24h.
        let should = match last {
            None => true,
            Some(t) => now.signed_duration_since(t) >= chrono::Duration::hours(24),
        };
        if !should {
            return;
        }
        // Go: defer writeLastCheckTime(currentTime) — written even if the fetch fails.
        let _ = std::fs::write(
            self.paths.last_check_version_file(),
            now.format("%+").to_string(),
        );
        // Fetch with 5s timeout.
        let body = ureq::Agent::new_with_defaults()
            .get(LATEST_VERSION_URL)
            .config()
            .timeout_per_call(Some(Duration::from_secs(5)))
            .build()
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_to_string().ok());
        let Some(body) = body else { return };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
            return;
        };
        let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else {
            return;
        };
        if semver_gt(tag, CURRENT_VERSION) {
            notify_update_available(tag);
        }
    }

    // ------------------------------------------------------------------
    // Layout (Go setHeightValues + updateComponentDimensions)
    // ------------------------------------------------------------------

    fn relayout(&mut self) {
        self.footer_h = footer_height(self.full_h, self.toggle_footer);
        let footer_total = if self.toggle_footer { self.footer_h + 2 } else { 0 };
        self.main_panel_h = self.full_h.saturating_sub(2).saturating_sub(footer_total);

        // Help menu: full screen minus border, capped at 90x30 on large terminals.
        let hm_h = if self.full_h > 35 { 30 } else { self.full_h.saturating_sub(2) };
        let hm_w = if self.full_w > 95 { 90 } else { self.full_w.saturating_sub(2) };
        self.help_menu.set_dimensions(hm_w, hm_h);
        self.prompt.set_dimensions((self.full_w / 2) as u16, (self.full_h / 3) as u16);
        self.zoxide.set_dimensions((self.full_w / 2) as u16, (self.full_h / 2) as u16);

        // Footer components: thirds of the width, footer height + borders.
        let f_h = self.footer_h + 2;
        let w1 = self.full_w / 3;
        let w2 = self.full_w / 3;
        let w3 = self.full_w - w1 - w2;
        self.processbar.set_dimensions(w1, f_h);
        self.metadata.set_dimensions(w2, f_h);
        self.clipboard_ui.set_dimensions(w3, f_h);

        // Sidebar + file model.
        let sidebar_w = self.config.sidebar_width as usize;
        self.sidebar.set_dimensions(sidebar_w + 2, self.main_panel_h + 2);
        let file_model_w = if sidebar_w > 0 {
            // saturating: a 0-width terminal (odd pty) must not panic.
            self.full_w.saturating_sub(sidebar_w + 2)
        } else {
            self.full_w
        };
        self.group
            .set_dimensions(file_model_w, self.main_panel_h + 2, self.preview.is_open(), &self.config);
        self.preview
            .set_dimensions(self.group.expected_preview_width() as u16, (self.main_panel_h + 2) as u16);
    }

    // ------------------------------------------------------------------
    // Rendering (Go viewContent + mainComponentsRender + updateRenderForOverlay)
    // ------------------------------------------------------------------

    fn render(&mut self, buf: &mut RBuf) {
        if self.full_h < 24 || self.full_w < 60 {
            self.render_size_warn_pre(buf);
            return;
        }
        if self.group.single_panel_width() < 18 {
            self.render_size_warn_post(buf);
            return;
        }
        let sidebar_w = self.config.sidebar_width as usize;
        let set = self.config.border_set();
        if sidebar_w > 0 {
            let loc = self.group.focused_panel().location.display().to_string();
            self.sidebar.draw(
                buf,
                0,
                0,
                sidebar_w + 2,
                self.main_panel_h + 2,
                self.focus == Focus::Sidebar,
                &loc,
                &self.pal,
                &self.ui,
                &self.hotkeys,
                set,
            );
        }
        let fm_x = if sidebar_w > 0 { sidebar_w + 2 } else { 0 };
        self.group.draw(buf, fm_x, 0, &self.pal, &self.ui, &self.config, &self.hotkeys);
        if self.preview.is_open() {
            let pw = self.group.expected_preview_width();
            if pw > 0 {
                self.preview.draw(
                    buf,
                    self.full_w - pw,
                    0,
                    pw,
                    self.main_panel_h + 2,
                    false,
                    &self.pal,
                    &self.ui,
                    set,
                );
            }
        }
        if self.toggle_footer {
            let fy = self.main_panel_h + 2;
            let fh = self.footer_h + 2;
            let w1 = self.full_w / 3;
            let w2 = self.full_w / 3;
            let w3 = self.full_w - w1 - w2;
            self.processbar
                .draw(buf, 0, fy, w1, fh, self.focus == Focus::ProcessBar, &self.pal, self.ui, set);
            self.metadata.draw(buf, w1, fy, w2, fh, self.focus == Focus::Metadata, &self.pal, set);
            self.clipboard_ui.draw(buf, w1 + w2, fy, w3, fh, &self.pal, &self.ui, set);
        }
        self.render_overlays(buf);
    }

    /// Go updateRenderForOverlay — exact order:
    /// spfError -> help -> prompt -> zoxide -> sort -> theme -> firstUse -> typing -> notify.
    /// Only ONE overlay renders at a time; centered at (full_w/2 - w/2, full_h/2 - h/2).
    fn render_overlays(&mut self, buf: &mut RBuf) {
        let cx = |w: usize| self.full_w / 2 - w / 2;
        let cy = |h: usize| self.full_h / 2 - h / 2;
        let set = self.config.border_set();
        if self.spf_error.is_open() {
            self.spf_error.draw(buf, cx(MODAL_W), cy(MODAL_H), &self.pal, &self.hotkeys, set);
            return;
        }
        if self.help_menu.is_open() {
            let (w, h) = (self.help_menu.width(), self.help_menu.height());
            self.help_menu.draw(buf, cx(w), cy(h), &self.pal, &self.ui, &self.hotkeys, set);
            return;
        }
        if self.prompt.is_open() {
            let (w, h) = (self.prompt.width(), self.prompt.height());
            self.prompt.draw(buf, cx(w), cy(h), w, h, true, &self.pal, &self.ui, &self.hotkeys, set);
            return;
        }
        if self.zoxide.is_open() {
            let (w, h) = (self.zoxide.width(), self.zoxide.height());
            self.zoxide.draw(buf, cx(w), cy(h), w, h, true, &self.pal, &self.ui, &self.hotkeys, set);
            return;
        }
        if self.sort_menu.is_open() {
            self.sort_menu.draw(buf, cx(SORT_W), cy(SORT_H), &self.pal, &self.ui, set);
            return;
        }
        if self.theme_menu.is_open() {
            // ThemeMenu is never resized by the app (Go doesn't either) -> 34x7.
            self.theme_menu.draw(buf, cx(THEME_W), cy(7), &self.pal, &self.ui, set);
            return;
        }
        if self.first_use {
            let (w, h) = (self.help_menu.width(), self.help_menu.height());
            dialogs::draw_first_use(buf, cx(w), cy(h), w, h, &self.pal, set);
            return;
        }
        if self.typing_modal.is_open() {
            self.typing_modal.draw(buf, cx(MODAL_W), cy(MODAL_H), &self.pal, &self.ui, &self.hotkeys, set);
            return;
        }
        if self.notify.is_open() {
            self.notify.draw(buf, cx(MODAL_W), cy(MODAL_H), &self.pal, &self.hotkeys, set);
        }
    }

    /// Go terminalSizeWarnRender (pre-first-render: fixed minimums 60x24).
    fn render_size_warn_pre(&self, buf: &mut RBuf) {
        let fg = self.pal.full_screen_fg;
        let w_col = if self.full_w < 60 { self.pal.error } else { self.pal.cursor };
        let h_col = if self.full_h < 24 { self.pal.error } else { self.pal.cursor };
        let ws = self.full_w.to_string();
        let hs = self.full_h.to_string();
        put_w(buf, 0, 0, "Terminal size too small:", fg);
        let mut x = put_w(buf, 0, 1, "Width = ", fg);
        x = put_w(buf, x, 1, &ws, w_col);
        x = put_w(buf, x, 1, " Height = ", fg);
        put_w(buf, x, 1, &hs, h_col);
        put_w(buf, 0, 3, "Needed for current config:", fg);
        let mut x = put_w(buf, 0, 4, "Width = ", fg);
        x = put_w(buf, x, 4, "60", self.pal.cursor);
        x = put_w(buf, x, 4, " Height = ", fg);
        put_w(buf, x, 4, "24", self.pal.cursor);
    }

    /// Go terminalSizeWarnAfterFirstRender (minimums computed from the config).
    fn render_size_warn_post(&self, buf: &mut RBuf) {
        let min_w = self.config.sidebar_width as usize + 20 * self.group.count() + 20 - 1;
        let fg = self.pal.full_screen_fg;
        let w_col = if self.full_w < min_w { self.pal.error } else { self.pal.cursor };
        let h_col = if self.full_h < 24 { self.pal.error } else { self.pal.cursor };
        let ws = self.full_w.to_string();
        let hs = self.full_h.to_string();
        let mws = min_w.to_string();
        put_w(buf, 0, 0, "Your terminal size is too small:", fg);
        let mut x = put_w(buf, 0, 1, "Width = ", fg);
        x = put_w(buf, x, 1, &ws, w_col);
        x = put_w(buf, x, 1, " Height = ", fg);
        put_w(buf, x, 1, &hs, h_col);
        put_w(buf, 0, 3, "Needed for current config:", fg);
        let mut x = put_w(buf, 0, 4, "Width = ", fg);
        x = put_w(buf, x, 4, &mws, self.pal.cursor);
        x = put_w(buf, x, 4, " Height = ", fg);
        put_w(buf, x, 4, "24", self.pal.cursor);
    }

    // ------------------------------------------------------------------
    // Key dispatch (Go handleKeyInput + mainKey + per-modal handlers)
    // ------------------------------------------------------------------

    fn handle_key(&mut self, k: &Key) {
        if self.first_use {
            self.first_use = false;
            return;
        }
        // Runs on EVERY key, BEFORE dispatch (Go: sidebarCmd before handleKeyInput).
        self.sidebar.update_state(k);

        let mut cd_on_quit = self.config.cd_on_quit;

        if self.spf_error.is_open() {
            match self.spf_error.handle_key(k, &self.hotkeys) {
                dialogs::SpfErrorAction::Skip => {
                    // Go spfErrorModelOpenKey -> FileListErrorState.Skip.
                    let remaining = self.spf_error.close();
                    self.skip_op(remaining);
                }
                dialogs::SpfErrorAction::Abort => {
                    // Go spfErrorModelOpenKey -> FileListErrorState.Abort.
                    let remaining = self.spf_error.close();
                    let _ = remaining;
                    self.abort_op();
                }
                dialogs::SpfErrorAction::None => {}
            }
            return;
        }
        if self.typing_modal.is_open() {
            match self.typing_modal.handle_key(k, &self.hotkeys) {
                dialogs::TypingAction::Cancel => {} // component already closed
                dialogs::TypingAction::Confirm => {
                    // Go getCreateCmd: close the modal, then run the create
                    // op on the typed name (validation failures surface via
                    // the worker Fail -> OpFail -> spf-error modal).
                    let value = self.typing_modal.value().to_string();
                    let location = PathBuf::from(self.typing_modal.location());
                    self.typing_modal.close();
                    if value.is_empty() {
                        return;
                    }
                    let items = vec![create_target(&location, &value)];
                    let total = fileops::op_total(OpKind::Create, &items);
                    self.spawn_op(OpKind::Create, items, location, total, false);
                }
                dialogs::TypingAction::None => {}
            }
            return;
        }
        if self.prompt.is_open() {
            // Ignored in Go's handleKeyInput; handled here via prompt.handle_key.
            self.prompt.set_cwd(self.group.focused_panel().location.clone());
            match self.prompt.handle_key(k, &self.hotkeys) {
                PromptAction::None => {}
                PromptAction::Close => self.prompt.close(),
                PromptAction::RunShell(cmd) => self.spawn_shell(cmd),
                PromptAction::RunSpf(line) => self.run_spf_line(line),
            }
            return;
        }
        if self.zoxide.is_open() {
            match self.zoxide.handle_key(k, &self.hotkeys) {
                crate::zoxide::ZoxideAction::None => {}
                crate::zoxide::ZoxideAction::Close => self.zoxide.close(),
                crate::zoxide::ZoxideAction::Cd(path) => {
                    self.zoxide.close();
                    let _ = self.cd_panel(PathBuf::from(path));
                }
            }
            return;
        }
        if self.notify.is_open() {
            match self.notify.handle_key(k, &self.hotkeys) {
                dialogs::NotifyAction::None => {}
                dialogs::NotifyAction::Cancel => match self.notify.action() {
                    ConfirmAction::Rename => {
                        self.group.focused_panel_mut().cancel_rename();
                    }
                    ConfirmAction::Quit => {
                        self.quit_state = QuitState::NotQuitting;
                    }
                    ConfirmAction::Delete | ConfirmAction::PermanentDelete | ConfirmAction::NoAction => {
                        // Do nothing
                    }
                },
                dialogs::NotifyAction::Confirm => match self.notify.action() {
                    ConfirmAction::Rename => self.confirm_rename(),
                    ConfirmAction::Quit => {
                        self.quit_state = QuitState::ConfirmReceived;
                        self.quit_cd_on_quit = cd_on_quit;
                    }
                    ConfirmAction::Delete => self.spawn_delete_op(true),
                    ConfirmAction::PermanentDelete => self.spawn_delete_op(false),
                    ConfirmAction::NoAction => {}
                },
            }
            return;
        }
        if self.group.focused_panel().rename_active {
            if keys::matches_any(&self.hotkeys.cancel_typing, k) {
                self.group.focused_panel_mut().cancel_rename();
                return;
            }
            if keys::matches_any(&self.hotkeys.confirm_typing, k) {
                if self.is_renaming_conflicting() {
                    self.notify.open(
                        "Confirm to rename file".into(),
                        "This file/directory name already exists. Are you sure you want to rename it? (Overwrite existing file/directory)".into(),
                        ConfirmAction::Rename,
                    );
                } else {
                    self.confirm_rename();
                }
                return;
            }
            self.group.focused_panel_mut().rename_buf.handle_key(k);
            return;
        }
        if self.sidebar.is_renaming() {
            if keys::matches_any(&self.hotkeys.cancel_typing, k) {
                self.sidebar.cancel_rename();
                return;
            }
            if keys::matches_any(&self.hotkeys.confirm_typing, k) {
                let _ = self.sidebar.confirm_rename();
                return;
            }
            return;
        }
        if self.group.focused_panel().search_active {
            if keys::matches_any(&self.hotkeys.cancel_typing, k) {
                self.group.focused_panel_mut().cancel_search();
                return;
            }
            if keys::matches_any(&self.hotkeys.confirm_typing, k) {
                self.group.focused_panel_mut().confirm_search();
                return;
            }
            let p = self.group.focused_panel_mut();
            p.search_buf.handle_key(k);
            let val = p.search_buf.value().to_string();
            p.apply_search_input(&val);
            return;
        }
        if self.sidebar.search_focused() {
            if keys::matches_any(&self.hotkeys.cancel_typing, k) {
                self.sidebar.cancel_search();
                return;
            }
            if keys::matches_any(&self.hotkeys.confirm_typing, k) {
                let _ = self.sidebar.confirm_search();
                return;
            }
            return;
        }
        if self.theme_menu.is_open() {
            match self.theme_menu.handle_key(k, &self.hotkeys) {
                dialogs::ThemeAction::None => {}
                dialogs::ThemeAction::Close => {}
                dialogs::ThemeAction::Confirm(i) => {
                    let name = self.theme_menu.themes().get(i).cloned().unwrap_or_default();
                    self.confirm_theme(name);
                }
            }
            return;
        }
        if self.sort_menu.is_open() {
            match self.sort_menu.handle_key(k, &self.hotkeys) {
                dialogs::SortAction::None => {}
                dialogs::SortAction::Close => {}
                dialogs::SortAction::Confirm(i) => {
                    let p = self.group.focused_panel_mut();
                    p.sort = SortKind::from_u8(i as u8);
                    p.refilter();
                }
            }
            return;
        }
        if self.help_menu.is_open() {
            let _ = self.help_menu.handle_key(k, &self.hotkeys); // swallows ALL keys
            return;
        }
        if keys::matches_any(&self.hotkeys.quit, k) {
            self.quit_state = QuitState::Initiated;
        } else if keys::matches_any(&self.hotkeys.cd_quit, k) {
            self.quit_state = QuitState::Initiated;
            cd_on_quit = true;
        } else {
            self.main_key(k);
        }

        // Go quit-state progression (runs after the switch in handleKeyInput).
        if self.quit_state == QuitState::Initiated {
            if self.processbar.has_running() {
                self.quit_state = QuitState::ConfirmInitiated;
                self.notify.open(
                    "Confirm to quit superfile".into(),
                    "You still have files being processed. Are you sure you want to exit?".into(),
                    ConfirmAction::Quit,
                );
            } else {
                self.quit_state = QuitState::ConfirmReceived;
                self.quit_cd_on_quit = cd_on_quit;
            }
        }
    }

    fn main_key(&mut self, k: &Key) {
        if keys::matches_any(&self.hotkeys.list_up, k) {
            self.list_up();
            return;
        }
        if keys::matches_any(&self.hotkeys.list_down, k) {
            self.list_down();
            return;
        }
        if keys::matches_any(&self.hotkeys.page_up, k) {
            match self.focus {
                Focus::Metadata => self.metadata.page_up(&self.config),
                Focus::None => {
                    self.group
                        .focused_panel_mut()
                        .page_up(self.config.page_scroll_size)
                }
                _ => {}
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.page_down, k) {
            match self.focus {
                Focus::Metadata => self.metadata.page_down(&self.config),
                Focus::None => {
                    self.group
                        .focused_panel_mut()
                        .page_down(self.config.page_scroll_size)
                }
                _ => {}
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.change_panel_mode, k) {
            self.group.focused_panel_mut().change_mode();
            return;
        }
        if keys::matches_any(&self.hotkeys.next_file_panel, k) {
            if self.focus == Focus::None {
                self.group.focus_next();
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.previous_file_panel, k) {
            if self.focus == Focus::None {
                self.group.focus_prev();
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.close_file_panel, k) {
            let _ = self.group.close_focused(); // Err("minimum panel count reached") ignored
            return;
        }
        if keys::matches_any(&self.hotkeys.create_new_file_panel, k) {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            let _ = self.group.add_panel(home, &self.config);
            return;
        }
        if keys::matches_any(&self.hotkeys.split_file_panel, k) {
            let _ = self.group.split(&self.config);
            return;
        }
        if keys::matches_any(&self.hotkeys.toggle_file_preview_panel, k) {
            self.toggle_preview();
            return;
        }
        if keys::matches_any(&self.hotkeys.focus_on_sidebar, k) {
            self.focus_on_sidebar();
            return;
        }
        if keys::matches_any(&self.hotkeys.focus_on_process_bar, k) {
            self.focus_on_process_bar();
            return;
        }
        if keys::matches_any(&self.hotkeys.focus_on_metadata, k) {
            self.focus_on_metadata();
            return;
        }
        if keys::matches_any(&self.hotkeys.paste_items, k) {
            self.paste_items();
            return;
        }
        if keys::matches_any(&self.hotkeys.file_panel_item_create, k) {
            let loc = self.group.focused_panel().location.display().to_string();
            self.typing_modal.open(loc);
            return;
        }
        if keys::matches_any(&self.hotkeys.pinned_directory, k) {
            let dir = self.group.focused_panel().location.display().to_string();
            let _ = self.sidebar.toggle_pinned(&dir);
            return;
        }
        if keys::matches_any(&self.hotkeys.toggle_dot_file, k) {
            self.display_dot_files = !self.display_dot_files;
            let _ = std::fs::write(
                self.paths.toggle_dot_file(),
                if self.display_dot_files { "true" } else { "false" },
            );
            for p in self.group.panels_mut() {
                p.dirty = true;
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.toggle_footer, k) {
            self.toggle_footer = !self.toggle_footer;
            let _ = std::fs::write(
                self.paths.toggle_footer_file(),
                if self.toggle_footer { "true" } else { "false" },
            );
            self.relayout();
            return;
        }
        if keys::matches_any(&self.hotkeys.extract_file, k) {
            self.extract_trigger();
            return;
        }
        if keys::matches_any(&self.hotkeys.compress_file, k) {
            self.compress_trigger();
            return;
        }
        if keys::matches_any(&self.hotkeys.open_command_line, k) {
            self.prompt.open(true);
            return;
        }
        if keys::matches_any(&self.hotkeys.open_spf_prompt, k) {
            self.prompt.open(false);
            return;
        }
        if keys::matches_any(&self.hotkeys.open_zoxide, k) {
            self.zoxide.open();
            return;
        }
        if keys::matches_any(&self.hotkeys.open_help_menu, k) {
            self.help_menu.open();
            return;
        }
        if keys::matches_any(&self.hotkeys.open_sort_options_menu, k) {
            self.sort_menu.open(self.group.focused_panel().sort as usize);
            return;
        }
        if keys::matches_any(&self.hotkeys.open_theme_menu, k) {
            let themes = Theme::list(&self.paths.theme_dir());
            self.theme_menu.open(themes, self.config.theme.clone());
            return;
        }
        if keys::matches_any(&self.hotkeys.toggle_reverse_sort, k) {
            let p = self.group.focused_panel_mut();
            p.sort_rev = !p.sort_rev;
            p.refilter();
            return;
        }
        if keys::matches_any(&self.hotkeys.open_file_with_editor, k) {
            self.open_with_editor(false);
            return;
        }
        if keys::matches_any(&self.hotkeys.open_current_directory_with_editor, k) {
            self.open_with_editor(true);
            return;
        }

        // normalAndBrowserModeKey.
        if self.focus != Focus::None {
            // unfocusedFilePanelKey — only the sidebar focus does anything.
            if self.focus == Focus::Sidebar {
                if keys::matches_any(&self.hotkeys.confirm, k) {
                    self.sidebar_select_directory();
                }
                if keys::matches_any(&self.hotkeys.file_panel_item_rename, k) {
                    self.sidebar.pinned_item_rename();
                }
                if keys::matches_any(&self.hotkeys.search_bar, k) {
                    self.sidebar_search_focus();
                }
            }
            return;
        }
        if self.group.focused_panel().mode == crate::panel::PanelMode::Select {
            // Select mode.
            if keys::matches_any(&self.hotkeys.confirm, k) {
                self.group.focused_panel_mut().single_item_select();
            }
            if keys::matches_any(&self.hotkeys.file_panel_select_mode_items_select_up, k) {
                self.group.focused_panel_mut().item_select_up();
            }
            if keys::matches_any(&self.hotkeys.file_panel_select_mode_items_select_down, k) {
                self.group.focused_panel_mut().item_select_down();
            }
            if keys::matches_any(&self.hotkeys.delete_items, k) {
                self.delete_trigger(false);
            }
            if keys::matches_any(&self.hotkeys.permanently_delete_items, k) {
                self.delete_trigger(true);
            }
            if keys::matches_any(&self.hotkeys.copy_items, k) {
                self.copy_multiple_items(false);
            }
            if keys::matches_any(&self.hotkeys.cut_items, k) {
                self.copy_multiple_items(true);
            }
            if keys::matches_any(&self.hotkeys.copy_path, k) {
                self.copy_path();
            }
            if keys::matches_any(&self.hotkeys.file_panel_select_all_items, k) {
                self.group.focused_panel_mut().select_all();
            }
            return;
        }
        // Normal (browser) mode.
        if keys::matches_any(&self.hotkeys.confirm, k) {
            self.enter_panel();
            return;
        }
        if keys::matches_any(&self.hotkeys.parent_directory, k) {
            let _ = self.group.focused_panel_mut().parent();
            return;
        }
        if keys::matches_any(&self.hotkeys.delete_items, k) {
            self.delete_trigger(false);
            return;
        }
        if keys::matches_any(&self.hotkeys.permanently_delete_items, k) {
            self.delete_trigger(true);
            return;
        }
        if keys::matches_any(&self.hotkeys.copy_items, k) {
            self.copy_single_item(false);
            return;
        }
        if keys::matches_any(&self.hotkeys.cut_items, k) {
            self.copy_single_item(true);
            return;
        }
        if keys::matches_any(&self.hotkeys.file_panel_item_rename, k) {
            if !self.group.focused_panel().empty() {
                self.group.focused_panel_mut().start_rename();
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.search_bar, k) {
            let active = self.group.focused_panel().search_active;
            let p = self.group.focused_panel_mut();
            if active {
                p.confirm_search();
            } else {
                p.focus_search();
            }
            return;
        }
        if keys::matches_any(&self.hotkeys.copy_path, k) {
            self.copy_path();
            return;
        }
        if keys::matches_any(&self.hotkeys.copy_present_working_directory, k) {
            let dir = self.group.focused_panel().location.display().to_string();
            let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(&dir));
            return;
        }
    }

    // ------------------------------------------------------------------
    // Focus / movement helpers
    // ------------------------------------------------------------------

    fn list_up(&mut self) {
        match self.focus {
            Focus::Sidebar => self.sidebar.list_up(),
            Focus::ProcessBar => self.processbar.list_up(),
            Focus::Metadata => self.metadata.list_up(),
            Focus::None => self.group.focused_panel_mut().list_up(),
        }
    }

    fn list_down(&mut self) {
        match self.focus {
            Focus::Sidebar => self.sidebar.list_down(),
            Focus::ProcessBar => self.processbar.list_down(),
            Focus::Metadata => self.metadata.list_down(),
            Focus::None => self.group.focused_panel_mut().list_down(),
        }
    }

    fn wheel_up(&mut self) {
        for _ in 0..5 {
            match self.focus {
                Focus::Sidebar => self.sidebar.list_up(),
                Focus::ProcessBar => self.processbar.list_up(),
                Focus::Metadata => self.metadata.list_up(),
                Focus::None => self.group.focused_panel_mut().list_up(),
            }
        }
    }

    fn wheel_down(&mut self) {
        for _ in 0..5 {
            match self.focus {
                Focus::Sidebar => self.sidebar.list_down(),
                Focus::ProcessBar => self.processbar.list_down(),
                Focus::Metadata => self.metadata.list_down(),
                Focus::None => self.group.focused_panel_mut().list_down(),
            }
        }
    }

    fn focus_on_sidebar(&mut self) {
        if self.config.sidebar_width == 0 {
            return;
        }
        self.focus = if self.focus == Focus::Sidebar {
            Focus::None
        } else {
            Focus::Sidebar
        };
    }

    fn focus_on_process_bar(&mut self) {
        if !self.toggle_footer {
            return;
        }
        self.focus = if self.focus == Focus::ProcessBar {
            Focus::None
        } else {
            Focus::ProcessBar
        };
    }

    fn focus_on_metadata(&mut self) {
        if !self.toggle_footer {
            return;
        }
        self.focus = if self.focus == Focus::Metadata {
            Focus::None
        } else {
            Focus::Metadata
        };
    }

    fn sidebar_select_directory(&mut self) {
        if self.sidebar.no_actual_dir() {
            return;
        }
        self.focus = Focus::None;
        if let Some(loc) = self.sidebar.current_location() {
            let _ = self.group.focused_panel_mut().cd(PathBuf::from(loc));
        }
    }

    fn sidebar_search_focus(&mut self) {
        if self.sidebar.search_focused() {
            self.sidebar.search_bar_blur();
            return;
        }
        self.sidebar.search_bar_focus();
    }

    fn cd_panel(&mut self, dir: PathBuf) -> Result<(), String> {
        self.group.focused_panel_mut().cd(dir)
    }

    // ------------------------------------------------------------------
    // Navigation / open actions
    // ------------------------------------------------------------------

    fn enter_panel(&mut self) {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return;
        }
        let Some(item) = panel.cursor_entry() else {
            return;
        };
        if item.is_dir {
            let mut target = item.path.clone();
            if item.is_symlink {
                // Go: after EvalSymlinks, Lstat must be a dir.
                match std::fs::canonicalize(&target) {
                    Ok(c) => match std::fs::symlink_metadata(&c) {
                        Ok(mi) if mi.is_dir() => target = c,
                        _ => return,
                    },
                    Err(_) => return,
                }
            }
            let _ = self.group.focused_panel_mut().cd(target);
            return;
        }
        // File: chooser-file behavior first.
        if let Some(cf) = &self.chooser_file {
            if chooser_write(cf, &item.path) {
                self.quit_state = QuitState::Initiated;
                return;
            }
            // Fall through to open on error (Go: continue with open).
        }
        self.open_with_xdg(&item.path);
    }

    /// Go executeOpenCommand: xdg-open (linux) or the configured open-with editor
    /// for the file's extension; detached, fire-and-forget.
    fn open_with_xdg(&self, path: &Path) {
        let mut open_command = "xdg-open".to_string();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if let Some(cmd) = self.config.open_with.get(&ext) {
            open_command = cmd.clone();
        }
        let _ = std::process::Command::new(open_command)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    /// Go openFileWithEditor / openDirectoryWithEditor, with the tea.ExecProcess
    /// emulation (restore terminal, run the editor to completion, restore again).
    fn open_with_editor(&mut self, dir_mode: bool) {
        let panel = self.group.focused_panel();
        if !dir_mode && panel.empty() {
            return;
        }
        if let Some(cf) = &self.chooser_file {
            let target = if dir_mode {
                panel.location.clone()
            } else {
                panel
                    .cursor_entry()
                    .map(|e| e.path.clone())
                    .unwrap_or_else(|| panel.location.clone())
            };
            if chooser_write(cf, &target) {
                self.quit_state = QuitState::Initiated;
                return;
            }
        }
        let editor_cfg = if dir_mode {
            self.config.dir_editor.clone()
        } else {
            self.config.editor.clone()
        };
        let editor = if editor_cfg.is_empty() {
            std::env::var("EDITOR").unwrap_or_default()
        } else {
            editor_cfg
        };
        let editor = if editor.is_empty() { "nano".to_string() } else { editor };
        let parts: Vec<&str> = editor.split_whitespace().collect();
        if parts.is_empty() {
            return;
        }
        let target = if dir_mode {
            panel.location.clone()
        } else {
            panel
                .cursor_entry()
                .map(|e| e.path.clone())
                .unwrap_or_else(|| panel.location.clone())
        };
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen, Show);
        let _ = std::process::Command::new(parts[0])
            .args(&parts[1..])
            .arg(&target)
            .status();
        let _ = terminal::enable_raw_mode();
        let _ = execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide);
        for p in self.group.panels_mut() {
            p.dirty = true; // listing may have changed (Go: UpdateFilePanelsIfNeeded(true))
        }
    }

    /// Go: copyPathText — empty panel -> ""; Select mode with selections ->
    /// selected locations joined with "\n" in visible order; otherwise the
    /// focused item.
    fn copy_path_text(&self) -> String {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return String::new();
        }
        if panel.mode == PanelMode::Select && panel.selected_count() > 0 {
            return panel
                .selected_paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
        }
        panel
            .cursor_entry()
            .map(|e| e.path.display().to_string())
            .unwrap_or_default()
    }

    fn copy_path(&mut self) {
        let text = self.copy_path_text();
        if text.is_empty() {
            return; // Go: copyPath early-return on empty text
        }
        let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(&text));
    }

    fn toggle_preview(&mut self) {
        if self.preview.is_open() {
            self.preview.close();
            self.preview_closed_by_user = true;
        } else {
            self.preview_closed_by_user = false;
            // Opening: the next update_preview() spawns content for the focused item.
            if let Some(item) = self.group.focused_panel().cursor_entry() {
                let path = item.path.clone();
                self.io_req += 1;
                self.preview.open_at(path, self.io_req);
            }
        }
        self.relayout();
    }

    // ------------------------------------------------------------------
    // Rename / theme
    // ------------------------------------------------------------------

    fn is_renaming_conflicting(&self) -> bool {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return false;
        }
        let Some(item) = panel.cursor_entry() else {
            return false;
        };
        let name = panel.rename_buf.value();
        if name.is_empty() {
            return false;
        }
        let new_path = panel.location.join(name);
        if new_path == item.path {
            return false;
        }
        new_path.exists()
    }

    fn confirm_rename(&mut self) {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return;
        }
        let Some(target) = panel.rename_target() else {
            self.group.focused_panel_mut().cancel_rename();
            return;
        };
        let old = panel.rename_orig.clone();
        let _ = std::fs::rename(&old, &target); // Go logs the error; state resets regardless
        let p = self.group.focused_panel_mut();
        p.cancel_rename();
        p.dirty = true; // the listing is now stale (Rust has no cursor-validity auto-relist)
    }

    fn confirm_theme(&mut self, name: String) {
        if name.is_empty() {
            self.theme_menu.close();
            return;
        }
        match Theme::load(&self.paths.theme_dir(), &name) {
            Ok(theme) => {
                self.theme = theme;
                self.pal = self.theme.resolve();
                self.config.theme = name;
                let _ = self.config.save(&self.paths.config_file_path());
                self.theme_menu.close();
            }
            Err(_) => {
                self.theme_menu.set_error("Could not apply theme. Selection unchanged.".into());
            }
        }
    }

    // ------------------------------------------------------------------
    // Worker orchestration (Task A scope)
    // ------------------------------------------------------------------

    /// Go fileModel.UpdateFilePanelsIfNeeded(false) — relist panels that are stale.
    fn relist_panels(&mut self) {
        let focus_idx = self.group.focused_idx();
        let focus_none = self.focus == Focus::None;
        let show = self.display_dot_files;
        let tx = self.tx.clone();
        for (i, panel) in self.group.panels_mut().iter_mut().enumerate() {
            if panel.needs_relist(focus_none && i == focus_idx) {
                self.io_req += 1;
                panel.spawn_list(self.io_req, show, tx.clone());
            }
        }
    }

    // ------------------------------------------------------------------
    // File operations (Task B)
    // ------------------------------------------------------------------

    /// Go copySingleItem — copy/cut the focused item into the clipboard
    /// (normal/browser mode).
    fn copy_single_item(&mut self, cut: bool) {
        self.clipboard_ui.reset(cut);
        let panel = self.group.focused_panel();
        if panel.empty() {
            return;
        }
        if let Some(e) = panel.cursor_entry() {
            self.clipboard_ui.add(&e.path.display().to_string());
        }
    }

    /// Go copyMultipleItem — copy/cut all selected items, in visible order
    /// (select mode).
    fn copy_multiple_items(&mut self, cut: bool) {
        self.clipboard_ui.reset(cut);
        let panel = self.group.focused_panel();
        if panel.selected_count() == 0 {
            return;
        }
        let items: Vec<String> = panel
            .selected_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.clipboard_ui.set_items(&items);
    }

    /// Go getPasteItemCmd + validatePasteOperation.
    fn paste_items(&mut self) {
        let items: Vec<PathBuf> = self
            .clipboard_ui
            .prune_inaccessible_and_get()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        if items.is_empty() {
            return;
        }
        let cut = self.clipboard_ui.is_cut();
        let dest = self.group.focused_panel().location.clone();
        if let Err(err) = fileops::validate_paste_operation(&dest, &items, cut) {
            self.notify.open("Invalid paste location".to_string(), err, ConfirmAction::NoAction);
            return;
        }
        let kind = if cut { OpKind::Cut } else { OpKind::Copy };
        let total = fileops::op_total(kind, &items);
        self.spawn_op(kind, items, dest, total, false);
    }

    /// Go `getDeleteTriggerCmd`: guard the panel state, then open the notify
    /// modal with the trash or permanent-delete warning.
    fn delete_trigger(&mut self, perm: bool) {
        let panel = self.group.focused_panel();
        if (panel.mode == crate::panel::PanelMode::Select && panel.selected_count() == 0)
            || (panel.mode == crate::panel::PanelMode::Browser && panel.empty())
        {
            return;
        }
        let trash_ok = fileops::has_trash() && fileops::trash_available_for(&panel.location);
        let (title, content, action) = delete_confirm(perm, trash_ok);
        self.notify.open(title.to_string(), content.to_string(), action);
    }

    /// Go `getDeleteCmd` (run after the notify confirm): collect the items
    /// (select mode: selected, visible order; browser mode: focused item)
    /// and spawn the delete op. `allow_trash` is the confirmed action —
    /// `Delete` (trash warning) or `PermanentDelete`.
    fn spawn_delete_op(&mut self, allow_trash: bool) {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return;
        }
        let items: Vec<PathBuf> = if panel.mode == crate::panel::PanelMode::Select {
            panel.selected_paths()
        } else {
            panel.cursor_entry().map(|e| vec![e.path.clone()]).unwrap_or_default()
        };
        let location = panel.location.clone();
        let use_trash = allow_trash && fileops::has_trash() && fileops::trash_available_for(&location);
        // Go: dest is unused by the delete processor; the panel location is
        // kept for the OpRecord.
        let total = fileops::op_total(OpKind::Delete, &items);
        self.spawn_op(OpKind::Delete, items, location, total, use_trash);
    }

    /// Go `getCompressSelectedFilesCmd`: sources are the selected items in
    /// visible order, or the focused item when nothing is selected; the zip
    /// name comes from the first source's base name.
    fn compress_trigger(&mut self) {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return;
        }
        let sources: Vec<PathBuf> = if panel.selected_count() > 0 {
            panel.selected_paths()
        } else {
            panel.cursor_entry().map(|e| vec![e.path.clone()]).unwrap_or_default()
        };
        let location = panel.location.clone();
        match plan_compress(&location, &sources) {
            CompressPlan::Notify { title, content } => {
                self.notify.open(title.to_string(), content, ConfirmAction::NoAction);
            }
            CompressPlan::AlreadyExists { target, total } => {
                // Revised R6 (unique_target exhaustion): add the process,
                // then immediately finish it Cancelled.
                self.io_req += 1;
                let op_id = self.io_req;
                let name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let _ = self
                    .processbar
                    .add(op_id, Operation::Compress, &name, total as u64);
                self.processbar
                    .finish(op_id, State::Cancelled, Some("File already exists".to_string()));
            }
            CompressPlan::Spawn { target, total } => {
                self.spawn_op(OpKind::Compress, sources, target, total, false);
            }
        }
    }

    /// Go `getExtractFileCmd`: the focused item, gated on its lowercased
    /// last extension; output dir = `file_name_without_extension` +
    /// `unique_target`, created before the worker starts (Go `MkdirAll`).
    fn extract_trigger(&mut self) {
        let panel = self.group.focused_panel();
        if panel.empty() {
            return;
        }
        let item = match panel.cursor_entry() {
            Some(e) => e.path.clone(),
            None => return,
        };
        if !fileops::is_extractable(&item) {
            return; // Go: logs and returns no cmd for non-extractable extensions
        }
        let dest = fileops::unique_target(fileops::file_name_without_extension(&item));
        if std::fs::create_dir_all(&dest).is_err() {
            return; // Go: MkdirAll failure -> Failed msg without a process entry
        }
        let items = vec![item];
        let total = fileops::op_total(OpKind::Extract, &items);
        self.spawn_op(OpKind::Extract, items, dest, total, false);
    }

    /// Go executePasteOperation et al. — register the process-bar entry,
    /// spawn the worker, and bridge its `WorkerMsg`s to the UI as `AsyncMsg`s
    /// (`Progress -> OpProgress`, `Fail -> OpFail`, `Done -> OpDone`).
    /// `total` is the process total (Go `op_total` for paste/delete/create,
    /// the compress validation count for Compress).
    fn spawn_op(
        &mut self,
        kind: OpKind,
        items: Vec<PathBuf>,
        dest: PathBuf,
        total: usize,
        use_trash: bool,
    ) {
        if items.is_empty() {
            return;
        }
        self.io_req += 1;
        let op_id = self.io_req;
        let cancel = Arc::new(AtomicBool::new(false));
        // Go process names: compress shows the zip target, everything else
        // the first source's base name.
        let name_src = if kind == OpKind::Compress { &dest } else { &items[0] };
        let name = name_src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| name_src.display().to_string());

        let (wtx, wrx) = mpsc::channel::<WorkerMsg>();
        fileops::spawn_worker(
            kind, op_id, items, dest.clone(), cancel.clone(), wtx, total, 0, use_trash,
        );
        self.bridge_worker(wrx, cancel.clone());

        let _ = self
            .processbar
            .add(op_id, processbar_operation(kind), &name, total as u64);
        self.ops.insert(
            op_id,
            OpRecord {
                kind,
                dest,
                cancel,
                total,
                use_trash,
                err: String::new(),
            },
        );
    }

    /// Forward a worker's `WorkerMsg`s into the UI channel (also used when
    /// resuming after Skip).
    fn bridge_worker(&mut self, wrx: mpsc::Receiver<WorkerMsg>, cancel_flag: Arc<AtomicBool>) {
        let utx = self.tx.clone();
        std::thread::spawn(move || {
            for msg in wrx.iter() {
                let ui = match msg {
                    WorkerMsg::Progress { op, file, done, total } => {
                        AsyncMsg::OpProgress { op, file, done, total }
                    }
                    WorkerMsg::Fail { op, file, err, remaining } => {
                        AsyncMsg::OpFail { op, file, err, remaining }
                    }
                    WorkerMsg::Done { op } => {
                        AsyncMsg::OpDone { op, cancelled: cancel_flag.load(Ordering::Relaxed) }
                    }
                };
                if utx.send(ui).is_err() {
                    return;
                }
            }
        });
    }

    /// Go `FileListErrorState.Skip`: resume the failed op with the remaining
    /// items under the same process id (R4: the process-bar entry stays
    /// in-operation). An empty list aborts instead (R5); progress continues
    /// from `total - remaining.len()` (R5) with the original total.
    fn skip_op(&mut self, remaining: Vec<String>) {
        if remaining.is_empty() {
            self.abort_op();
            return;
        }
        let op_id = match self.error_op.take() {
            Some(id) => id,
            None => return,
        };
        let (kind, dest, cancel, total, use_trash) = match self.ops.get(&op_id) {
            Some(rec) => (
                rec.kind,
                rec.dest.clone(),
                rec.cancel.clone(),
                rec.total,
                rec.use_trash,
            ),
            None => return,
        };
        let done_offset = total.saturating_sub(remaining.len());
        let (wtx, wrx) = mpsc::channel::<WorkerMsg>();
        fileops::spawn_remaining(
            kind,
            op_id,
            remaining,
            dest,
            cancel.clone(),
            wtx,
            total,
            done_offset,
            use_trash,
        );
        self.bridge_worker(wrx, cancel);
    }

    /// Go `FileListErrorState.Abort`: cancel the worker, finish the process
    /// Failed with the error stored by `OpFail`, and drop the record.
    fn abort_op(&mut self) {
        apply_abort(&mut self.processbar, &mut self.ops, &mut self.error_op);
    }

    fn handle_async(&mut self, msg: AsyncMsg) {
        match msg {
            AsyncMsg::DirListed { path, entries, .. } => {
                for panel in self.group.panels_mut() {
                    if panel.location == path {
                        panel.apply_list(&path, entries.clone());
                    }
                }
            }
            AsyncMsg::OpProgress { op, file, done, total } => {
                self.processbar
                    .update_progress(op, &file, done as u64, total as u64);
            }
            AsyncMsg::OpFail { op, file, err, remaining } => {
                // R4: keep the process-bar entry in-operation and keep the
                // OpRecord until the user skips or aborts; the error message
                // is stored for the abort path.
                let _ = file;
                if let Some(rec) = self.ops.get_mut(&op) {
                    rec.err = err.clone();
                }
                self.error_op = Some(op);
                self.spf_error.open(err, remaining);
            }
            AsyncMsg::OpDone { op, cancelled } => {
                let state = if cancelled { State::Cancelled } else { State::Successful };
                self.processbar.finish(op, state, None);
                if let Some(rec) = self.ops.remove(&op) {
                    let panel = self.group.focused_panel_mut();
                    apply_op_effects(&mut self.clipboard_ui, rec.kind, state, panel);
                }
                // R9: refresh every panel so pasted/moved entries show up.
                for p in self.group.panels_mut() {
                    p.dirty = true;
                }
            }
            AsyncMsg::Metadata { .. } => {}
            AsyncMsg::Md5 { .. } => {}
            AsyncMsg::ZoxideQuery { req_id, results } => {
                self.zoxide.set_results(req_id, results);
            }
            AsyncMsg::ShellDone { ret_code, output, .. } => {
                // Go HandleShellCommandResults.
                let trimmed = util::make_printable(&output);
                let trimmed = trimmed.trim();
                let msg_text = if trimmed.is_empty() {
                    format!("Command exited with status {ret_code} (No output)")
                } else {
                    format!("Command exited with status {ret_code}, Output:\n{trimmed}")
                };
                self.prompt.set_result(
                    ret_code == 0,
                    &msg_text,
                    self.config.shell_close_on_success,
                );
            }
            AsyncMsg::Preview { req_id, path, content } => {
                self.preview.apply(req_id, &path, content);
            }
            AsyncMsg::UpdateCheck { .. } => {}
            // `$(cmd)` substitution finished in a worker thread: execute the
            // resolved SPF line, or surface the substitution error.
            AsyncMsg::SubstDone { req_id, resolved } => {
                let _ = req_id;
                match resolved {
                    Ok(line) => self.run_spf_line(line),
                    Err(e) => self.prompt.set_result(false, &e, false),
                }
            }
        }
    }

    /// Go MetadataMsg.ApplyToModel — stale-guarded.
    fn handle_metadata(&mut self, msg: metadata::MetadataMsg) {
        let focused = self.focus == Focus::Metadata;
        if focused != self.metadata.get_expected_focused() {
            return;
        }
        if msg.data.filepath != self.metadata.get_location() {
            return;
        }
        self.metadata.set_metadata(msg.data, focused);
    }

    fn dispatch_zoxide_query(&mut self) {
        if !self.zoxide.is_open() || !self.zoxide.needs_query() {
            return;
        }
        // Availability was checked at startup; if the binary vanished, the command
        // just fails -> empty results.
        let value = self.zoxide.value().to_string();
        self.io_req += 1;
        let req = self.io_req;
        self.zoxide.mark_queried(req);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = std::process::Command::new("zoxide")
                .args(["query", "--interactive"])
                .arg(&value)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .output();
            let results: Vec<(f64, String)> = match out {
                Ok(o) if o.status.success() => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    text.lines()
                        .filter_map(|line| {
                            let mut parts = line.splitn(2, '\t');
                            let score = parts.next()?.parse::<f64>().ok()?;
                            let path = parts.next()?.to_string();
                            Some((score, path))
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            let _ = tx.send(AsyncMsg::ZoxideQuery { req_id: req, results });
        });
    }

    /// Prompt shell mode: /bin/sh -c in the focused panel's directory, 5s timeout
    /// (Go utils.ExecuteCommandInShell), in a worker thread so the UI doesn't freeze.
    fn spawn_shell(&mut self, cmd: String) {
        let cwd = self.group.focused_panel().location.clone();
        self.io_req += 1;
        let req = self.io_req;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (ret_code, output) = run_shell_timed(&cwd, &cmd);
            let _ = tx.send(AsyncMsg::ShellDone {
                req_id: req,
                ret_code,
                output,
                err: None,
            });
        });
    }

    /// SPF prompt line execution (synchronous — resolve_substitutions already ran
    /// inside prompt.handle_key).
    fn run_spf_line(&mut self, line: String) {
        let cwd = self.group.focused_panel().location.clone();
        let tokens = match prompt::tokenize_with_quotes(&line) {
            Ok(t) => t,
            Err(e) => {
                self.prompt
                    .set_result(false, &format!("Failed during tokenization : {e}"), false);
                return;
            }
        };
        let spf = match prompt::parse_spf_line(&tokens) {
            Ok(c) => c,
            Err(e) => {
                self.prompt.set_result(false, &e, false);
                return;
            }
        };
        let res: Result<String, String> = match spf {
            SpfCmd::Split => {
                self.group
                    .split(&self.config)
                    .map(|_| "Panel successfully split".to_string())
            }
            SpfCmd::Cd(p) => {
                let target = resolve_abs(&cwd, &p);
                self.cd_panel(target)
                    .map(|_| "Panel directory changed".to_string())
            }
            SpfCmd::Open(p) => {
                let target = resolve_abs(&cwd, &p);
                self.group
                    .add_panel(target, &self.config)
                    .map(|_| "New panel opened".to_string())
            }
        };
        match res {
            Ok(msg) => self.prompt.set_result(true, &msg, self.config.shell_close_on_success),
            Err(e) => self.prompt.set_result(false, &e, false),
        }
    }

    /// Go fileModel.GetFilePreviewCmd(false) + ensurePreviewDimensionsSync semantics.
    fn update_preview(&mut self) {
        if !self.preview.is_open() {
            // Go: preview starts open when config.default_open_file_preview
            // (and the user hasn't closed it).
            if !self.preview_closed_by_user && self.config.default_open_file_preview {
                let Some(item) = self.group.focused_panel().cursor_entry() else {
                    return
                };
                let path = item.path.clone();
                self.io_req += 1;
                self.preview.open_at(path, self.io_req);
                // Go ToggleFilePreviewPanel: updateChildComponentWidth right
                // after the open state flips, so the panel widths and the
                // preview width used below are fresh.
                self.relayout();
            } else {
                return;
            }
        }
        let panel = self.group.focused_panel();
        let Some(item) = panel.cursor_entry() else {
            self.preview.set_empty();
            return;
        };
        let pw = self.group.expected_preview_width();
        let pad = if self.config.enable_file_preview_border { 2 } else { 0 };
        let cw = pw.saturating_sub(pad);
        let ch = (self.main_panel_h + 2).saturating_sub(pad);
        let path = item.path.clone();
        let same = self.preview.current_path() == Some(path.as_path());
        let dims_stale = self.preview_spawn_dims != (cw, ch);
        if !same || dims_stale {
            self.io_req += 1;
            let req = self.io_req;
            self.preview.open_at(path.clone(), req);
            self.preview_spawn_dims = (cw, ch);
            preview::spawn_preview(req, path, &self.config, &self.ui, cw, ch, self.tx.clone());
        }
    }

    /// Go getMetadataCmd.
    fn update_metadata(&mut self) {
        // Go getMetadataCmd has no config gate (its `disableMetadata` field is
        // test-only). The `metadata` config flag only gates the exiftool
        // fields inside the worker (Go updateExiftoolMetadata; Rust
        // metadata.rs collect_metadata), so the pane must fetch basic
        // metadata even when the flag is false.
        let panel = self.group.focused_panel();
        if panel.empty() {
            self.metadata.set_blank();
            return;
        }
        let Some(item) = panel.cursor_entry() else {
            self.metadata.set_blank();
            return;
        };
        let focused = self.focus == Focus::Metadata;
        let loc = item.path.display().to_string();
        if loc == self.metadata.get_location() && focused == self.metadata.get_expected_focused() {
            return;
        }
        self.metadata.set_location_and_focused(&loc, focused);
        if self.metadata.is_blank() {
            let msg = format!("{}{}Loading metadata...", self.ui.in_operation, self.ui.space);
            self.metadata.set_info_msg(&msg);
        }
        self.io_req += 1;
        metadata::spawn_metadata(self.io_req, loc, focused, self.config.clone(), self.md_tx.clone());
    }
}

// ----------------------------------------------------------------------
// Free functions
// ----------------------------------------------------------------------

/// Map an op kind to its process-bar operation type.
/// Outcome of the Go `getCompressSelectedFilesCmd` pre-flight checks
/// (`getZipArchiveName` incl. `renameIfDuplicate` + `validateCompressOperation`
/// + the revised R6 exhaustion rule).
enum CompressPlan {
    /// Spawn the Compress worker for this explicit zip target.
    Spawn { target: PathBuf, total: usize },
    /// Notify modal (Go "Invalid zip target name" / "Invalid file/dir to
    /// compress").
    Notify { title: &'static str, content: String },
    /// Revised R6: `unique_target` exhausted its counter and the target
    /// still exists — add the process, finish it Cancelled.
    AlreadyExists { target: PathBuf, total: usize },
}

fn plan_compress(location: &Path, sources: &[PathBuf]) -> CompressPlan {
    let base = sources
        .first()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let zip_name = match fileops::zip_archive_name(&base) {
        Ok(n) => n,
        Err(err) => {
            return CompressPlan::Notify {
                title: "Invalid zip target name",
                content: err,
            }
        }
    };
    // Revised R6 (Go `getZipArchiveName` -> `renameIfDuplicate`): a duplicate
    // zip target renames to `name(N).zip`; Cancelled only on counter
    // exhaustion (unique_target returns the input while it exists).
    let target = fileops::unique_target(location.join(&zip_name));
    let total = match fileops::validate_compress_sources(sources) {
        Ok(t) => t,
        Err(err) => {
            return CompressPlan::Notify {
                title: "Invalid file/dir to compress",
                content: err,
            }
        }
    };
    if target.exists() {
        // unique_target exhausted (Go renameIfDuplicate failure).
        return CompressPlan::AlreadyExists { target, total };
    }
    CompressPlan::Spawn { target, total }
}

/// Core of Go `FileListErrorState.Abort`: cancel the worker, finish the
/// process Failed with the error stored by `OpFail`, and drop the record.
/// (The worker already stopped when it sent `Fail`, so no `OpDone` follows.)
fn apply_abort(
    processbar: &mut ProcessBar,
    ops: &mut HashMap<u64, OpRecord>,
    error_op: &mut Option<u64>,
) {
    let op_id = match error_op.take() {
        Some(id) => id,
        None => return,
    };
    if let Some(rec) = ops.remove(&op_id) {
        rec.cancel.store(true, Ordering::Relaxed);
        processbar.finish(op_id, State::Failed, Some(rec.err));
    }
}

fn processbar_operation(kind: OpKind) -> Operation {
    match kind {
        OpKind::Copy => Operation::Copy,
        OpKind::Cut => Operation::Cut,
        OpKind::Delete => Operation::Delete,
        OpKind::Create => Operation::Create,
        OpKind::Compress => Operation::Compress,
        OpKind::Extract => Operation::Extract,
    }
}

/// Go `PasteOperationMsg.ApplyToModel` — a completed (successful or failed,
/// but not cancelled) cut paste clears the clipboard. A cut paste is exactly
/// a `Cut` op with a cut clipboard; other op kinds leave it untouched.
fn apply_paste_effects(clipboard: &mut Clipboard, kind: OpKind, state: State) {
    if kind == OpKind::Cut && state != State::Cancelled && clipboard.is_cut() {
        clipboard.reset(false);
    }
}

/// Go `filepath.Join(location, typed)` parity: the typed value is always
/// appended under the panel location, even when it starts with `/` (Rust
/// `PathBuf::join` would *replace* the base for an absolute argument).
fn create_target(location: &Path, typed: &str) -> PathBuf {
    location.join(typed.trim_start_matches('/'))
}

/// Go `getDeleteTriggerCmd` wording selection: trash warning when the trash
/// is usable and the hotkey was not the permanent-delete one, else the
/// permanent warning.
fn delete_confirm(perm: bool, trash_ok: bool) -> (&'static str, &'static str, ConfirmAction) {
    if trash_ok && !perm {
        (TRASH_WARN_TITLE, TRASH_WARN_CONTENT, ConfirmAction::Delete)
    } else {
        (
            PERM_DELETE_WARN_TITLE,
            PERM_DELETE_WARN_CONTENT,
            ConfirmAction::PermanentDelete,
        )
    }
}

/// Post-completion effects of an op (Go `*OperationMsg.ApplyToModel`):
/// paste ops update the clipboard; a delete op clears the focused panel's
/// selection (`DeleteOperationMsg`: ResetSelected on every state).
fn apply_op_effects(
    clipboard: &mut Clipboard,
    kind: OpKind,
    state: State,
    panel: &mut crate::panel::FilePanel,
) {
    apply_paste_effects(clipboard, kind, state);
    if kind == OpKind::Delete {
        panel.reset_selected();
    }
}

/// Go setHeightValues footer logic.
fn footer_height(full_h: usize, toggle: bool) -> usize {
    if !toggle {
        0
    } else if full_h < 30 {
        6
    } else if full_h < 35 {
        7
    } else if full_h < 40 {
        8
    } else if full_h < 45 {
        9
    } else {
        10
    }
}

/// Go utils.ReadBoolFile: "true" -> true, "false" -> false; any error/garbage -> default.
fn read_bool_file(path: &Path, default: bool) -> bool {
    match std::fs::read_to_string(path) {
        Ok(s) => match s.trim() {
            "true" => true,
            "false" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

/// Whether a binary is on PATH (Go: zClient init success ~= binary present).
fn which_bin(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Go chooserFileWriteAndQuit file-write: write the path's raw bytes to the file.
fn chooser_write(cf: &Path, path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::fs::write(cf, path.as_os_str().as_bytes()).is_ok()
}

fn resolve_abs(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Go utils.ExecuteCommandInShell port: /bin/sh -c, dir, 5s deadline,
/// combined stdout+stderr, retCode -1 on timeout/spawn failure.
fn run_shell_timed(cwd: &Path, cmd: &str) -> (i32, String) {
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (-1, format!("unexpected Error in command execution : {e}")),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (txo, rxo) = mpsc::channel::<Vec<u8>>();
    if let Some(mut so) = stdout {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            use std::io::Read;
            let _ = so.read_to_end(&mut b);
            let _ = txo.send(b);
        });
    }
    let (txe, rxe) = mpsc::channel::<Vec<u8>>();
    if let Some(mut se) = stderr {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            use std::io::Read;
            let _ = se.read_to_end(&mut b);
            let _ = txe.send(b);
        });
    }
    let res = crate::fileops::wait_timeout(child, Duration::from_secs(5));
    let out = rxo.recv().unwrap_or_default();
    let err = rxe.recv().unwrap_or_default();
    let mut combined = String::from_utf8_lossy(&out).to_string();
    let err_text = String::from_utf8_lossy(&err);
    if !err_text.is_empty() {
        combined.push_str(&err_text);
    }
    match res {
        Ok(Some(status)) => (status.code().unwrap_or(-1), combined),
        Ok(None) => (-1, combined),
        Err(e) => (-1, format!("unexpected Error in command execution : {e}")),
    }
}

fn read_last_check_time(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(content.trim())
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// true when a ("vX.Y.Z") is strictly greater than b ("vX.Y.Z");
/// any parse mismatch -> false.
fn semver_gt(a: &str, b: &str) -> bool {
    let p = |s: &str| {
        let s = s.trim().trim_start_matches('v');
        let mut it = s.split('.');
        let major = it.next()?.parse::<u64>().ok()?;
        let minor = it.next()?.parse::<u64>().ok()?;
        let patch = it.next()?.parse::<u64>().ok()?;
        Some((major, minor, patch))
    };
    match (p(a), p(b)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// Go notifyUpdateAvailable — exact text (ANSI colors inline; the alt screen
/// has already been left by this point).
fn notify_update_available(latest: &str) {
    let p1 = "\u{1b}[38;2;255;105;225m"; // ┃
    let p2 = "\u{1b}[38;2;255;186;82;1m"; // A new version / is available.
    let p3 = "\u{1b}[38;2;0;255;242;1;3m"; // latest (bold italic)
    let r = "\u{1b}[0m";
    println!("{p1} ┃ {p2}A new version {p3}{latest}{p2} is available.{r}");
    println!("{p1} ┃ {r}Please update.\n┏\n\n      => {LATEST_VERSION_GH}\n\n");
    println!("                                                               ┛");
}

/// Draw a warning-text segment at (x, y) and return the next x.
fn put_w(buf: &mut RBuf, x: usize, y: usize, s: &str, c: Color) -> usize {
    let _ = buf.put_str(x, y, s, St::new().fg(c));
    x + util::str_width(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bool_file() {
        let dir =
            std::env::temp_dir().join(format!("spf_app_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("read_bool_file_test.txt");
        std::fs::write(&p, "true").unwrap();
        assert_eq!(read_bool_file(&p, false), true);
        std::fs::write(&p, "false").unwrap();
        assert_eq!(read_bool_file(&p, true), false);
        std::fs::write(&p, "garbage").unwrap();
        assert_eq!(read_bool_file(&p, true), true);
        assert_eq!(read_bool_file(&p, false), false);
        std::fs::remove_file(&p).ok();
        assert_eq!(read_bool_file(&p, false), false);
        assert_eq!(read_bool_file(&p, true), true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_update_metadata_dispatches_with_plugin_flag_off() {
        // Go: `metadata` in config.toml only gates the exiftool fields inside
        // the worker (updateExiftoolMetadata); the pane dispatch itself is
        // unconditional (getMetadataCmd). Rust must fetch basic metadata
        // even when the flag is false.
        let dir = std::env::temp_dir().join(format!("spf_md_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("item.txt"), "x").unwrap();
        std::fs::write(dir.join("config/config.toml"), "metadata = false\n").unwrap();

        let paths = Paths {
            config_dir: dir.join("config"),
            state_dir: dir.join("state"),
            data_dir: dir.join("data"),
            config_file: None,
            hotkey_file: None,
        };
        let mut app = App::new(paths, vec![dir.clone()], false, None);
        assert!(!app.config.metadata, "test requires the plugin flag off");

        // Populate the focused panel through the real async listing path.
        app.relist_panels();
        for _ in 0..50 {
            if let Ok(msg) = app.rx.recv_timeout(std::time::Duration::from_millis(100)) {
                app.handle_async(msg);
            }
            if app.group.focused_panel().cursor_entry().is_some() {
                break;
            }
        }
        assert!(
            app.group.focused_panel().cursor_entry().is_some(),
            "DirListed did not arrive"
        );

        app.update_metadata();

        // The fetch must be dispatched for the focused cursor item (worker
        // sends a MetadataMsg) and the pane must have adopted its location.
        let expected = app.group.focused_panel().cursor_entry().unwrap().path.clone();
        let msg = app
            .md_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("no MetadataMsg dispatched with metadata = false");
        assert_eq!(PathBuf::from(&msg.data.filepath), expected);
        assert_eq!(app.metadata.get_location(), expected.display().to_string());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build an App on a temp dir containing exactly 3 files, with the focused
    /// panel populated through the real async listing path.
    fn copy_app_fixture(tag: &str) -> (App, PathBuf) {
        let base = std::env::temp_dir().join(format!("spf_cp_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("files");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let paths = Paths {
            config_dir: base.join("config"),
            state_dir: base.join("state"),
            data_dir: base.join("data"),
            config_file: None,
            hotkey_file: None,
        };
        let mut app = App::new(paths, vec![dir.clone()], false, None);

        app.relist_panels();
        for _ in 0..50 {
            if let Ok(msg) = app.rx.recv_timeout(std::time::Duration::from_millis(100)) {
                app.handle_async(msg);
            }
            if app.group.focused_panel().entries.len() >= 3 {
                break;
            }
        }
        assert!(
            app.group.focused_panel().cursor_entry().is_some(),
            "DirListed did not arrive"
        );
        (app, base)
    }

    #[test]
    fn test_copy_path_text_select_mode_joins_selected() {
        // Go copyPathText: Select mode with selections -> selected locations
        // joined with "\n" in visible order.
        let (mut app, base) = copy_app_fixture("sel");
        let (first, third) = {
            let panel = app.group.focused_panel();
            (panel.entries[0].path.clone(), panel.entries[2].path.clone())
        };
        {
            let panel = app.group.focused_panel_mut();
            panel.mode = PanelMode::Select;
            panel.toggle_selected(&first);
            panel.toggle_selected(&third);
        }
        let expected = format!("{}\n{}", first.display(), third.display());
        assert_eq!(app.copy_path_text(), expected);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_copy_path_text_select_mode_empty_uses_cursor() {
        // Go copyPathText: Select mode with no selection -> focused item only.
        let (mut app, base) = copy_app_fixture("sel_empty");
        app.group.focused_panel_mut().mode = PanelMode::Select;
        let focused = app.group.focused_panel().cursor_entry().unwrap().path.clone();
        assert_eq!(app.copy_path_text(), focused.display().to_string());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_copy_path_text_browser_mode_uses_cursor() {
        // Go copyPathText ignores selections outside Select mode: stale selected
        // entries in Browser mode still copy only the focused item.
        let (mut app, base) = copy_app_fixture("browser");
        let (first, second) = {
            let panel = app.group.focused_panel();
            (panel.entries[0].path.clone(), panel.entries[1].path.clone())
        };
        {
            let panel = app.group.focused_panel_mut();
            panel.toggle_selected(&first);
            panel.toggle_selected(&second);
        }
        let focused = app.group.focused_panel().cursor_entry().unwrap().path.clone();
        assert_eq!(app.copy_path_text(), focused.display().to_string());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_semver_gt() {
        assert!(semver_gt("v1.7.0", "v1.6.0"));
        assert!(!semver_gt("v1.6.0", "v1.6.0"));
        assert!(semver_gt("v1.6.1", "v1.6.0"));
        assert!(!semver_gt("v1.5.9", "v1.6.0"));
        assert!(!semver_gt("not-a-version", "v1.6.0"));
        assert!(semver_gt("v2.0.0", "v1.6.0"));
    }

    #[test]
    fn test_footer_height() {
        assert_eq!(footer_height(100, false), 0);
        assert_eq!(footer_height(29, true), 6);
        assert_eq!(footer_height(34, true), 7);
        assert_eq!(footer_height(39, true), 8);
        assert_eq!(footer_height(44, true), 9);
        assert_eq!(footer_height(45, true), 10);
        assert_eq!(footer_height(100, true), 10);
    }

    #[test]
    fn test_resolve_abs() {
        assert_eq!(resolve_abs(Path::new("/a"), Path::new("/b")), PathBuf::from("/b"));
        assert_eq!(
            resolve_abs(Path::new("/a"), Path::new("b/c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn test_apply_paste_effects() {
        // Go PasteOperationMsg.ApplyToModel: a completed cut paste clears
        // the clipboard; cancelled ones and copy pastes do not.
        let mut clipboard = Clipboard::new();

        clipboard.reset(true);
        clipboard.add("/tmp/x");
        apply_paste_effects(&mut clipboard, OpKind::Cut, State::Successful);
        assert!(!clipboard.is_cut());
        assert!(clipboard.items().is_empty());

        clipboard.reset(true);
        clipboard.add("/tmp/x");
        apply_paste_effects(&mut clipboard, OpKind::Cut, State::Cancelled);
        assert!(clipboard.is_cut());
        assert_eq!(clipboard.items(), vec!["/tmp/x".to_string()]);

        clipboard.reset(true);
        clipboard.add("/tmp/x");
        apply_paste_effects(&mut clipboard, OpKind::Copy, State::Successful);
        assert!(clipboard.is_cut());
        assert_eq!(clipboard.items(), vec!["/tmp/x".to_string()]);

        // Delete ops leave the clipboard alone.
        clipboard.reset(true);
        clipboard.add("/tmp/x");
        apply_paste_effects(&mut clipboard, OpKind::Delete, State::Successful);
        assert!(clipboard.is_cut());
    }

    #[test]
    fn test_delete_confirm_wording() {
        // Go getDeleteTriggerCmd: trash warning when the trash is usable and
        // the hotkey was not the permanent one.
        let (title, content, action) = delete_confirm(false, true);
        assert_eq!(title, "Are you sure you want to move this to trash can");
        assert_eq!(content, "This operation will move file or directory to trash can.");
        assert_eq!(action, ConfirmAction::Delete);

        // Permanent hotkey or no usable trash -> permanent warning.
        for (perm, trash_ok) in [(true, true), (false, false), (true, false)] {
            let (title, content, action) = delete_confirm(perm, trash_ok);
            assert_eq!(title, "Are you sure you want to completely delete");
            assert_eq!(
                content,
                "This operation cannot be undone and your data will be completely lost."
            );
            assert_eq!(action, ConfirmAction::PermanentDelete);
        }
    }

    #[test]
    fn test_create_target_absolute_typed_appends_under_location() {
        // Review fix 2: Go filepath.Join appends the typed value under the
        // panel directory even when it starts with '/'.
        let loc = PathBuf::from("/tmp/panel");
        assert_eq!(create_target(&loc, "/etc/passwd"), PathBuf::from("/tmp/panel/etc/passwd"));
        assert_eq!(create_target(&loc, "a.txt"), PathBuf::from("/tmp/panel/a.txt"));
        assert_eq!(create_target(&loc, "//x"), PathBuf::from("/tmp/panel/x"));
        assert_eq!(create_target(&loc, "/"), PathBuf::from("/tmp/panel"));

        // The target really lands under the panel directory (create_item).
        let dir = std::env::temp_dir().join(format!("spf_create_abs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        fileops::create_item(&create_target(&dir, "/sub/x.txt")).unwrap();
        assert!(dir.join("sub/x.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_op_effects_delete_resets_selection() {
        // Go DeleteOperationMsg.ApplyToModel: a delete op (any state) clears
        // the focused panel's selection; other ops do not.
        use crate::fileops::Entry;
        use crate::panel::{FilePanel, PanelMode};
        let mut panel = FilePanel::new(PathBuf::from("/tmp"), &Config::default());
        panel.entries = vec![Entry {
            name: "a.txt".into(),
            path: PathBuf::from("/tmp/a.txt"),
            is_dir: false,
            is_symlink: false,
            size: 1,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            mode: 0o100644,
        }];
        panel.mode = PanelMode::Select;
        panel.selected = vec![PathBuf::from("/tmp/a.txt")];

        let mut clipboard = Clipboard::new();
        clipboard.reset(true);
        clipboard.add("/tmp/a.txt");

        apply_op_effects(&mut clipboard, OpKind::Delete, State::Successful, &mut panel);
        assert_eq!(panel.selected_count(), 0);
        // Delete ops leave the clipboard alone.
        assert!(clipboard.is_cut());
        assert_eq!(clipboard.items(), vec!["/tmp/a.txt".to_string()]);

        panel.selected = vec![PathBuf::from("/tmp/a.txt")];
        apply_op_effects(&mut clipboard, OpKind::Delete, State::Cancelled, &mut panel);
        assert_eq!(panel.selected_count(), 0);

        // A copy op does not clear the selection.
        panel.selected = vec![PathBuf::from("/tmp/a.txt")];
        apply_op_effects(&mut clipboard, OpKind::Copy, State::Successful, &mut panel);
        assert_eq!(panel.selected_count(), 1);
    }

    #[test]
    fn test_plan_compress_duplicate_target_renames() {
        // Revised R6 (Go `getZipArchiveName` -> `renameIfDuplicate`): a
        // duplicate zip target renames to a(1).zip; the existing zip is
        // untouched.
        let dir = std::env::temp_dir().join(format!("spf_compress_dup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.zip"), "old").unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();

        match plan_compress(&dir, &[dir.join("a.txt")]) {
            CompressPlan::Spawn { target, total } => {
                assert_eq!(target, dir.join("a(1).zip"));
                assert_eq!(total, 1);
            }
            _ => panic!("duplicate target must plan Spawn with renamed target"),
        }
        assert_eq!(std::fs::read_to_string(dir.join("a.zip")).unwrap(), "old");

        // Exhaustion: unique_target resumes from (9999), hits the 10_000 cap
        // and returns the input path while it exists -> Cancelled.
        std::fs::write(dir.join("a(9999).txt"), "x").unwrap();
        std::fs::write(dir.join("a(9999).zip"), "old").unwrap();
        match plan_compress(&dir, &[dir.join("a(9999).txt")]) {
            CompressPlan::AlreadyExists { target, total } => {
                assert_eq!(target, dir.join("a(9999).zip"));
                assert_eq!(total, 1);
            }
            _ => panic!("exhausted counter must plan AlreadyExists"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_plan_compress_naming_and_validation() {
        let dir = std::env::temp_dir().join(format!("spf_compress_plan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "x").unwrap();
        std::fs::create_dir_all(dir.join("tree/sub")).unwrap();
        std::fs::write(dir.join("tree/sub/b.txt"), "x").unwrap();

        // R10: dotfile base keeps its full name (.gitignore -> .gitignore.zip).
        match plan_compress(&dir, &[dir.join(".gitignore")]) {
            CompressPlan::Spawn { target, total } => {
                assert_eq!(target, dir.join(".gitignore.zip"));
                assert_eq!(total, 1);
            }
            _ => panic!(".gitignore must plan Spawn"),
        }
        // Recursive readable count feeds the process total.
        match plan_compress(&dir, &[dir.join("tree"), dir.join(".gitignore")]) {
            CompressPlan::Spawn { target, total } => {
                assert_eq!(target, dir.join("tree.zip"));
                assert_eq!(total, 2);
            }
            _ => panic!("tree must plan Spawn"),
        }
        // Go validateCompressOperation failure -> notify wording.
        match plan_compress(&dir, &[dir.join("nope.txt")]) {
            CompressPlan::Notify { title, content } => {
                assert_eq!(title, "Invalid file/dir to compress");
                assert!(content.contains("source path does not exist"), "{content}");
            }
            _ => panic!("missing source must plan Notify"),
        }
        // Go getZipArchiveName failure -> notify wording.
        match plan_compress(&dir, &[]) {
            CompressPlan::Notify { title, content } => {
                assert_eq!(title, "Invalid zip target name");
                assert_eq!(content, "empty filename to compress");
            }
            _ => panic!("empty base must plan Notify"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_abort_finishes_process_failed_with_message() {
        // Go FileListErrorState.Abort: the process-bar entry becomes Failed
        // with the error stored by OpFail, the worker's cancel flag is set,
        // and the OpRecord is dropped (R4 lifecycle end).
        let mut pb = ProcessBar::new();
        pb.add(7, processbar_operation(OpKind::Copy), "missing.txt", 3)
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ops: HashMap<u64, OpRecord> = HashMap::new();
        ops.insert(
            7,
            OpRecord {
                kind: OpKind::Copy,
                dest: PathBuf::from("/tmp/dest"),
                cancel: cancel.clone(),
                total: 3,
                use_trash: false,
                err: "copy failed: No such file or directory".to_string(),
            },
        );
        let mut error_op = Some(7u64);

        apply_abort(&mut pb, &mut ops, &mut error_op);

        assert!(cancel.load(Ordering::Relaxed));
        assert!(ops.is_empty());
        assert_eq!(error_op, None);
        assert_eq!(
            pb.process_status(7),
            Some((
                State::Failed,
                "copy failed: No such file or directory".to_string()
            ))
        );
        assert!(!pb.has_running());
    }
}
