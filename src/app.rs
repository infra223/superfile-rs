//! Application core: state, terminal lifecycle, event loop, layout, rendering,
//! key dispatch, and worker orchestration (port of superfile v1.6.0 internal/model.go
//! + model_render.go + key_function.go + handle_*.go, app-level pieces).
//!
//! Deviations (documented):
//! 1. Poll-based event loop (crossterm poll ~16ms) instead of bubbletea's channel loop;
//!    tea.Cmd/tea.Msg become std::thread + mpsc.
//! 2. File-operation keys (paste/copy/cut/delete/create-confirm/zip/extract) and the
//!    spf-error skip/abort flow are TASK B — wired as no-ops with `// TODO(task-B)` for now.
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

use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
use crate::icons::{self, Ui};
use crate::keys::{self, Key};
use crate::metadata;
use crate::panel::SortKind;
use crate::panels::PanelGroup;
use crate::preview;
use crate::processbar::ProcessBar;
use crate::prompt::{self, Prompt, PromptAction, SpfCmd};
use crate::render::{RBuf, St};
use crate::sidebar::Sidebar;
use crate::util;
use crate::zoxide::Zoxide;

const CURRENT_VERSION: &str = "v1.6.0"; // Go variable.CurrentVersion
const LATEST_VERSION_URL: &str = "https://api.github.com/repos/yorukot/superfile/releases/latest";
const LATEST_VERSION_GH: &str = "https://github.com/yorukot/superfile/releases";

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
    preview_spawn_dims: (usize, usize), // last (content w, content h) a preview was spawned with
    preview_closed_by_user: bool,
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
            self.full_w - (sidebar_w + 2)
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
                .draw(buf, 0, fy, w1, fh, self.focus == Focus::ProcessBar, &self.pal, self.ui);
            self.metadata.draw(buf, w1, fy, w2, fh, self.focus == Focus::Metadata, &self.pal);
            self.clipboard_ui.draw(buf, w1 + w2, fy, w3, fh, &self.pal, &self.ui);
        }
        self.render_overlays(buf);
    }

    /// Go updateRenderForOverlay — exact order:
    /// spfError -> help -> prompt -> zoxide -> sort -> theme -> firstUse -> typing -> notify.
    /// Only ONE overlay renders at a time; centered at (full_w/2 - w/2, full_h/2 - h/2).
    fn render_overlays(&mut self, buf: &mut RBuf) {
        let cx = |w: usize| self.full_w / 2 - w / 2;
        let cy = |h: usize| self.full_h / 2 - h / 2;
        if self.spf_error.is_open() {
            self.spf_error.draw(buf, cx(MODAL_W), cy(MODAL_H), &self.pal, &self.hotkeys);
            return;
        }
        if self.help_menu.is_open() {
            let (w, h) = (self.help_menu.width(), self.help_menu.height());
            self.help_menu.draw(buf, cx(w), cy(h), &self.pal, &self.ui, &self.hotkeys);
            return;
        }
        if self.prompt.is_open() {
            let (w, h) = (self.prompt.width(), self.prompt.height());
            self.prompt.draw(buf, cx(w), cy(h), w, h, true, &self.pal, &self.ui, &self.hotkeys);
            return;
        }
        if self.zoxide.is_open() {
            let (w, h) = (self.zoxide.width(), self.zoxide.height());
            self.zoxide.draw(buf, cx(w), cy(h), w, h, true, &self.pal, &self.ui, &self.hotkeys);
            return;
        }
        if self.sort_menu.is_open() {
            self.sort_menu.draw(buf, cx(SORT_W), cy(SORT_H), &self.pal, &self.ui);
            return;
        }
        if self.theme_menu.is_open() {
            // ThemeMenu is never resized by the app (Go doesn't either) -> 34x7.
            self.theme_menu.draw(buf, cx(THEME_W), cy(7), &self.pal, &self.ui);
            return;
        }
        if self.first_use {
            let (w, h) = (self.help_menu.width(), self.help_menu.height());
            dialogs::draw_first_use(buf, cx(w), cy(h), w, h, &self.pal);
            return;
        }
        if self.typing_modal.is_open() {
            self.typing_modal.draw(buf, cx(MODAL_W), cy(MODAL_H), &self.pal, &self.ui, &self.hotkeys);
            return;
        }
        if self.notify.is_open() {
            self.notify.draw(buf, cx(MODAL_W), cy(MODAL_H), &self.pal, &self.hotkeys);
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
                    let remaining = self.spf_error.close();
                    // TODO(task-B): resume worker with remaining (drop first; empty -> abort)
                    let _ = remaining;
                }
                dialogs::SpfErrorAction::Abort => {
                    let remaining = self.spf_error.close();
                    // TODO(task-B): abort worker (empty remaining)
                    let _ = remaining;
                }
                dialogs::SpfErrorAction::None => {}
            }
            return;
        }
        if self.typing_modal.is_open() {
            match self.typing_modal.handle_key(k, &self.hotkeys) {
                dialogs::TypingAction::Cancel => {} // component already closed
                dialogs::TypingAction::Confirm => {
                    let _value = self.typing_modal.value().to_string();
                    self.typing_modal.close();
                    // TODO(task-B): spawn create op with the typed name
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
                    ConfirmAction::Delete | ConfirmAction::PermanentDelete => {
                        // TODO(task-B): spawn delete op (soft / permanent)
                    }
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
            // TODO(task-B)
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
            // TODO(task-B)
            return;
        }
        if keys::matches_any(&self.hotkeys.compress_file, k) {
            // TODO(task-B)
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
                // TODO(task-B)
            }
            if keys::matches_any(&self.hotkeys.permanently_delete_items, k) {
                // TODO(task-B)
            }
            if keys::matches_any(&self.hotkeys.copy_items, k) {
                // TODO(task-B)
            }
            if keys::matches_any(&self.hotkeys.cut_items, k) {
                // TODO(task-B)
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
            // TODO(task-B)
            return;
        }
        if keys::matches_any(&self.hotkeys.permanently_delete_items, k) {
            // TODO(task-B)
            return;
        }
        if keys::matches_any(&self.hotkeys.copy_items, k) {
            // TODO(task-B)
            return;
        }
        if keys::matches_any(&self.hotkeys.cut_items, k) {
            // TODO(task-B)
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

    fn copy_path(&mut self) {
        let Some(item) = self.group.focused_panel().cursor_entry() else {
            return;
        };
        let _ = arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(&item.path.display().to_string()));
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
            AsyncMsg::OpFail { .. } => {
                // TODO(task-B): spf_error.open(err, remaining)
            }
            AsyncMsg::OpDone { .. } => {
                // TODO(task-B): finish op in the process bar
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
        if !self.config.metadata {
            return;
        }
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
}
