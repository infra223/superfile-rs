//! Sidebar: home / pinned / disks sections (configurable order), search bar,
//! pinned-item rename, pin/unpin.
//!
//! Exact port of `src/internal/ui/sidebar` from superfile v1.6.0.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, Hotkeys, Palette};
use crate::fuzzy;
use crate::icons::{icon_for, ui_icons, Ui};
use crate::keys::{Key, KeyKind};
use crate::render::{BorderSet, RBuf, St};
use crate::text_input::TextInput;
use crate::util::{str_width, truncate_end};

const BORDER_PADDING: usize = 2;
/// borders (2) + prompt (2) + extra char (1)
const SEARCH_BAR_PADDING: usize = 5;
const DIVIDER_DIR_HEIGHT: usize = 3;
/// superfile logo + blank line + search bar
const INITIAL_HEIGHT: usize = 3;
const DIVIDER_LENGTH: usize = 20;
const MIN_HEIGHT: usize = 5;
const MIN_WIDTH: usize = 7;
const CHAR_LIMIT: usize = 156;

const SECTION_HOME: &str = "home";
const SECTION_PINNED: &str = "pinned";
const SECTION_DISKS: &str = "disks";

/// Sentinel locations used to recognize section dividers.
const HOME_DIVIDER: &str = "Home+-*/=?";
const PINNED_DIVIDER: &str = "Pinned+-*/=?";
const DISK_DIVIDER: &str = "Disks+-*/=?";

/// Action reported back to the app after a sidebar text-input confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarAction {
    None,
    RenameDone,
    SearchBlur,
}

#[derive(Clone, Debug)]
struct Dir {
    location: String,
    name: String,
    section: &'static str,
    icon: String,
}

impl Dir {
    fn is_divider(&self) -> bool {
        matches!(self.location.as_str(), HOME_DIVIDER | PINNED_DIVIDER | DISK_DIVIDER)
    }

    fn required_height(&self) -> usize {
        if self.is_divider() {
            DIVIDER_DIR_HEIGHT
        } else {
            1
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PinnedItem {
    location: String,
    name: String,
}

pub struct Sidebar {
    directories: Vec<Dir>,
    render_index: usize,
    cursor: usize,
    rename: TextInput,
    renaming: bool,
    search: TextInput,
    search_focused: bool,
    pinned_file: PathBuf,
    width: usize,
    height: usize,
    disabled: bool,
    sections: Vec<String>,
    nerdfont: bool,
}

impl Sidebar {
    /// `pinned_file` is where the pinned list JSON lives
    /// (`Paths::pinned_file()`).
    pub fn new(config: &Config, pinned_file: PathBuf) -> Self {
        if config.sidebar_width == 0 {
            return Sidebar {
                directories: Vec::new(),
                render_index: 0,
                cursor: 0,
                rename: TextInput::new(),
                renaming: false,
                search: TextInput::new(),
                search_focused: false,
                pinned_file,
                width: 0,
                height: MIN_HEIGHT,
                disabled: true,
                sections: config.sidebar_sections.clone(),
                nerdfont: config.nerdfont,
            };
        }

        // Go NewPinnedFileManager → InitJSONFile: create "[]" when missing.
        if !pinned_file.exists() {
            let _ = fs::write(&pinned_file, "[]");
        }

        let width = config.sidebar_width as usize + BORDER_PADDING;
        let mut s = Sidebar {
            directories: Vec::new(),
            render_index: 0,
            cursor: 0,
            rename: TextInput::new(),
            renaming: false,
            search: TextInput::new(),
            search_focused: false,
            pinned_file,
            width,
            height: MIN_HEIGHT,
            disabled: false,
            sections: config.sidebar_sections.clone(),
            nerdfont: config.nerdfont,
        };
        s.search.width = width - BORDER_PADDING - SEARCH_BAR_PADDING;
        s.directories = s.build_directories();
        s
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// Feed a key into the active text input (rename wins over search).
    /// Never consumes; the app dispatches confirm/cancel separately.
    pub fn update_state(&mut self, k: &Key) {
        if self.renaming {
            self.feed_rename(k);
        } else if self.search_focused {
            self.feed_search(k);
        }
    }

    /// Go textinput CharLimit: new characters are rejected at the limit.
    fn feed_rename(&mut self, k: &Key) {
        if matches!(k.kind, KeyKind::Ch(c) if c != '\0') && self.rename.buf.chars().count() >= CHAR_LIMIT {
            return;
        }
        self.rename.handle_key(k);
    }

    /// Go textinput CharLimit: new characters are rejected at the limit.
    fn feed_search(&mut self, k: &Key) {
        if matches!(k.kind, KeyKind::Ch(c) if c != '\0') && self.search.buf.chars().count() >= CHAR_LIMIT {
            return;
        }
        self.search.handle_key(k);
    }

    /// Rebuild the directory list (search-filtered when a query is active).
    /// Called every frame; also re-loads pinned and mounts, like Go.
    pub fn update_directories(&mut self) {
        if self.disabled {
            return;
        }
        let query = self.search.value().to_string();
        self.directories = if query.is_empty() {
            self.build_directories()
        } else {
            self.build_filtered(&query)
        };
        // The cursor may be invalid after filtering.
        if self.is_cursor_invalid() {
            self.reset_cursor();
        }
    }

    // ---------------------------------------------------------------
    // Directory list construction
    // ---------------------------------------------------------------

    fn build_directories(&self) -> Vec<Dir> {
        let ui = ui_icons(self.nerdfont);
        let home = self.well_known_dirs(&ui);
        let pinned = self.pinned_dirs(&ui);
        let disks = self.disk_dirs(&ui);
        form_directory_slice(&home, &pinned, &disks, &self.sections)
    }

    fn build_filtered(&self, query: &str) -> Vec<Dir> {
        let ui = ui_icons(self.nerdfont);
        let home = self.well_known_dirs(&ui);
        let pinned = self.pinned_dirs(&ui);
        let disks = self.disk_dirs(&ui);
        let home: Vec<Dir> = fuzzy::filter_scored(query, &home, &|d: &Dir| d.name.clone())
            .into_iter()
            .cloned()
            .collect();
        let pinned: Vec<Dir> = fuzzy::filter_scored(query, &pinned, &|d: &Dir| d.name.clone())
            .into_iter()
            .cloned()
            .collect();
        let disks: Vec<Dir> = fuzzy::filter_scored(query, &disks, &|d: &Dir| d.name.clone())
            .into_iter()
            .cloned()
            .collect();
        form_directory_slice(&home, &pinned, &disks, &self.sections)
    }

    /// XDG user dirs that actually exist, in Go's fixed order.
    fn well_known_dirs(&self, ui: &Ui) -> Vec<Dir> {
        let entries: Vec<(Option<PathBuf>, &str, &str)> = vec![
            (dirs::home_dir(), ui.home, "Home"),
            (dirs::desktop_dir(), ui.desktop, "Desktop"),
            (dirs::download_dir(), ui.download, "Downloads"),
            (dirs::document_dir(), ui.documents, "Documents"),
            (dirs::picture_dir(), ui.pictures, "Pictures"),
            (dirs::video_dir(), ui.videos, "Videos"),
            (dirs::audio_dir(), ui.music, "Music"),
            (dirs::template_dir(), ui.templates, "Templates"),
            (dirs::home_dir().map(|h| h.join("Public")), ui.public_share, "PublicShare"),
        ];
        let mut out = Vec::new();
        for (loc, icon, name) in entries {
            if let Some(loc) = loc {
                if fs::metadata(&loc).is_ok() {
                    out.push(Dir {
                        location: loc.to_string_lossy().into_owned(),
                        name: name.to_string(),
                        section: SECTION_HOME,
                        icon: icon.to_string(),
                    });
                }
            }
        }
        // Trash: XDG_DATA_HOME/Trash (Linux only in Go).
        #[cfg(target_os = "linux")]
        if let Some(data) = dirs::data_dir() {
            let trash = data.join("Trash");
            if fs::metadata(&trash).is_ok() {
                out.push(Dir {
                    location: trash.to_string_lossy().into_owned(),
                    name: "Trash".to_string(),
                    section: SECTION_HOME,
                    icon: ui.trash.to_string(),
                });
            }
        }
        out
    }

    fn pinned_dirs(&self, ui: &Ui) -> Vec<Dir> {
        load_pinned_clean(&self.pinned_file)
            .into_iter()
            .map(|p| {
                let icon = icon_for(&p.name, true, false, ui, None).glyph;
                Dir {
                    location: p.location,
                    name: p.name,
                    section: SECTION_PINNED,
                    icon: icon.to_string(),
                }
            })
            .collect()
    }

    /// Mounted external media from /proc/mounts, filtered like Go's
    /// gopsutil + `shouldListDisk`.
    fn disk_dirs(&self, ui: &Ui) -> Vec<Dir> {
        let mounts = match fs::read_to_string("/proc/mounts") {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for line in mounts.lines() {
            let mut fields = line.split(' ');
            fields.next(); // device
            let Some(raw) = fields.next() else {
                continue;
            };
            let mp = decode_mount_escape(raw);
            if !should_list_disk(&mp) {
                continue;
            }
            let icon = if mp == "/" { ui.terminal } else { ui.disk };
            let name = if mp == "/" { "Root".to_string() } else { base_name(&mp) };
            out.push(Dir {
                location: mp,
                name: name.to_string(),
                section: SECTION_DISKS,
                icon: icon.to_string(),
            });
        }
        out
    }

    // ---------------------------------------------------------------
    // Navigation
    // ---------------------------------------------------------------

    pub fn list_up(&mut self) {
        if self.no_actual_dir() {
            return;
        }
        self.cursor = if self.cursor > 0 { self.cursor - 1 } else { self.directories.len() - 1 };
        // Update even if the cursor lands on a divider, otherwise dividers
        // are sometimes skipped in render for large pinned lists.
        self.update_render_index();
        if self.directories[self.cursor].is_divider() {
            // Skip dividers (Go recurses here; depth is bounded by the list).
            self.list_up();
        }
    }

    pub fn list_down(&mut self) {
        if self.no_actual_dir() {
            return;
        }
        self.cursor = if self.cursor < self.directories.len() - 1 {
            self.cursor + 1
        } else {
            0
        };
        self.update_render_index();
        if self.directories[self.cursor].is_divider() {
            self.list_down();
        }
    }

    /// Last index renderable when starting at `start`; `start - 1` when
    /// nothing fits.
    fn last_rendered_index(&self, start: usize) -> usize {
        let main_h = self.height.saturating_sub(BORDER_PADDING);
        let mut cur = INITIAL_HEIGHT;
        let mut end = start.saturating_sub(1);
        for i in start..self.directories.len() {
            cur += self.directories[i].required_height();
            if cur > main_h {
                break;
            }
            end = i;
        }
        end
    }

    /// First index renderable when ending at `end`; `end + 1` when nothing
    /// fits.
    fn first_rendered_index(&self, end: usize) -> usize {
        let main_h = self.height.saturating_sub(BORDER_PADDING);
        if end >= self.directories.len() {
            return end + 1;
        }
        let mut cur = INITIAL_HEIGHT;
        let mut start = end + 1;
        for i in (0..=end).rev() {
            cur += self.directories[i].required_height();
            if cur > main_h {
                break;
            }
            start = i;
        }
        start
    }

    fn update_render_index(&mut self) {
        if self.directories.is_empty() {
            self.render_index = 0;
            return;
        }
        // Case I: cursor moved above the current renderable range.
        if self.cursor < self.render_index {
            self.render_index = self.cursor;
            return;
        }
        let cur_end = self.last_rendered_index(self.render_index);
        // Case II: cursor still inside the rendered range.
        if self.render_index <= self.cursor && self.cursor <= cur_end {
            return;
        }
        // Case III: cursor too far below.
        if cur_end < self.cursor {
            self.render_index = self.first_rendered_index(self.cursor);
        }
    }

    fn reset_cursor(&mut self) {
        self.cursor = 0;
        for (i, d) in self.directories.iter().enumerate() {
            if !d.is_divider() {
                self.cursor = i;
                return;
            }
        }
    }

    // ---------------------------------------------------------------
    // State queries
    // ---------------------------------------------------------------

    pub fn is_renaming(&self) -> bool {
        self.renaming
    }

    pub fn search_focused(&self) -> bool {
        self.search_focused
    }

    /// True when the sidebar contains only dividers (or nothing).
    pub fn no_actual_dir(&self) -> bool {
        self.directories.iter().all(|d| d.is_divider())
    }

    fn is_cursor_invalid(&self) -> bool {
        self.cursor >= self.directories.len()
            || self
                .directories
                .get(self.cursor)
                .is_some_and(|d| d.is_divider())
    }

    /// Location of the selected directory, or None when the cursor is on a
    /// divider / no real directories exist.
    pub fn current_location(&self) -> Option<&str> {
        if self.is_cursor_invalid() || self.no_actual_dir() {
            None
        } else {
            Some(self.directories[self.cursor].location.as_str())
        }
    }

    // ---------------------------------------------------------------
    // Pinned item rename
    // ---------------------------------------------------------------

    /// Start renaming the selected pinned directory (no-op when the cursor
    /// is not inside the pinned section).
    pub fn pinned_item_rename(&mut self) {
        let Some((begin, end)) = self.pinned_index_range() else {
            return;
        };
        if self.cursor < begin || self.cursor > end {
            return;
        }
        self.renaming = true;
        self.rename = TextInput::with_value(self.directories[self.cursor].name.clone());
        self.rename.width = self.width.saturating_sub(BORDER_PADDING + 3);
    }

    fn pinned_index_range(&self) -> Option<(usize, usize)> {
        let mut begin: Option<usize> = None;
        let mut end: Option<usize> = None;
        for (i, d) in self.directories.iter().enumerate() {
            if d.section == SECTION_PINNED {
                if begin.is_none() {
                    begin = Some(i);
                }
                end = Some(i);
            }
        }
        match (begin, end) {
            (Some(b), Some(e)) => Some((b, e)),
            _ => None,
        }
    }

    pub fn cancel_rename(&mut self) {
        self.renaming = false;
    }

    /// Commit the rename into the pinned list (matched by location) and save.
    pub fn confirm_rename(&mut self) -> SidebarAction {
        if !self.renaming {
            return SidebarAction::None;
        }
        let item_location = self.directories[self.cursor].location.clone();
        let new_name = self.rename.value().to_string();
        // Update the in-memory entry, then recover rename state.
        self.directories[self.cursor].name = new_name.clone();
        self.renaming = false;

        // Reload the full (unfiltered) pinned list and commit by location.
        let mut pinned = load_pinned_clean(&self.pinned_file);
        for p in pinned.iter_mut() {
            if p.location == item_location {
                p.name = new_name.clone();
            }
        }
        let _ = save_pinned(&self.pinned_file, &pinned);
        SidebarAction::RenameDone
    }

    // ---------------------------------------------------------------
    // Search bar
    // ---------------------------------------------------------------

    pub fn search_bar_focus(&mut self) {
        self.search_focused = true;
    }

    pub fn search_bar_blur(&mut self) {
        self.search_focused = false;
    }

    pub fn set_search_value(&mut self, v: &str) {
        self.search.buf = v.to_string();
        self.search.cursor = v.len();
        self.search.scroll = 0;
    }

    /// Confirm: blur and reset the cursor (the filter stays active).
    pub fn confirm_search(&mut self) -> SidebarAction {
        self.search_focused = false;
        self.reset_cursor();
        SidebarAction::SearchBlur
    }

    /// Cancel: blur and clear the query.
    pub fn cancel_search(&mut self) {
        self.search_focused = false;
        self.search.buf.clear();
        self.search.cursor = 0;
        self.search.scroll = 0;
    }

    // ---------------------------------------------------------------
    // Pin / unpin
    // ---------------------------------------------------------------

    pub fn toggle_pinned(&mut self, dir: &str) -> Result<(), String> {
        let mut pinned = load_pinned_clean(&self.pinned_file);
        match pinned.iter().position(|p| p.location == dir) {
            Some(i) => {
                pinned.remove(i);
            }
            None => pinned.push(PinnedItem {
                location: dir.to_string(),
                name: base_name(dir),
            }),
        }
        save_pinned(&self.pinned_file, &pinned).map_err(|e| format!("error saving pinned directories: {e}"))
    }

    // ---------------------------------------------------------------
    // Dimensions
    // ---------------------------------------------------------------

    /// Go only guards the height (min 5); the width is fixed by the app.
    pub fn set_dimensions(&mut self, w: usize, h: usize) {
        self.width = w;
        if h >= MIN_HEIGHT {
            self.height = h;
        }
        self.search.width = w.saturating_sub(BORDER_PADDING).saturating_sub(SEARCH_BAR_PADDING);
        if self.renaming {
            self.rename.width = w.saturating_sub(BORDER_PADDING + 3);
        }
    }

    // ---------------------------------------------------------------
    // Render
    // ---------------------------------------------------------------

    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        focused: bool,
        current_file_panel_location: &str,
        pal: &Palette,
        ui: &Ui,
        hk: &Hotkeys,
    ) {
        if self.disabled {
            return;
        }
        buf.fill(x, y, w, h, St::fg_bg(pal.sidebar_bg, pal.sidebar_bg));
        if w < MIN_WIDTH || h < MIN_HEIGHT {
            return;
        }

        let border_fg = if focused {
            pal.sidebar_border_active
        } else {
            pal.sidebar_border
        };
        buf.border(x, y, w, h, BorderSet::plain(), border_fg, pal.sidebar_bg);

        let cx = x + 1;
        let cw = w.saturating_sub(2);
        let plain = St::fg_bg(pal.sidebar_fg, pal.sidebar_bg);
        let title_st = St::new().fg(pal.sidebar_title).bg(pal.sidebar_bg);

        // Line 0: " {superfile icon} superfile"
        buf.put_line(cx, y + 1, cw, "", plain);
        let _ = buf.put_str(cx, y + 1, &format!(" {} {}superfile", ui.superfile, ui.space), title_st);

        // Line 1: blank.
        let mut row = y + 3;

        // Line 2: search bar, shown when focused, non-empty, or sidebar focused.
        if self.search_focused || !self.search.value().is_empty() || focused {
            let prompt = format!("{}{}", ui.search, ui.space);
            buf.put_line(cx, row, cw, "", St::fg_bg(pal.file_panel_fg, pal.file_panel_bg));
            let px = buf.put_str(
                cx,
                row,
                &prompt,
                St::new().fg(pal.file_panel_top_dir_icon).bg(pal.file_panel_bg),
            );
            let view = if self.search.value().is_empty() {
                let first = hk.search_bar.first().cloned().unwrap_or_default();
                let ph = format!("({first}) Search");
                // Charm shows the placeholder after the cursor when focused.
                if self.search_focused {
                    format!("│{ph}")
                } else {
                    ph
                }
            } else {
                self.search.view(self.search_focused)
            };
            let _ = buf.put_str(px, row, &view, St::new().fg(pal.file_panel_fg).bg(pal.file_panel_bg));
            row += 1;
        }

        if self.no_actual_dir() {
            buf.put_line(cx, row, cw, "", plain);
            let _ = buf.put_str(cx, row, &format!(" {} {}None", ui.error, ui.space), plain);
            return;
        }

        // Directories from renderIndex while they fit.
        let main_h = self.height.saturating_sub(BORDER_PADDING);
        let mut total = INITIAL_HEIGHT;
        for i in self.render_index..self.directories.len() {
            let d = &self.directories[i];
            if total + d.required_height() > main_h {
                break;
            }
            total += d.required_height();

            if d.is_divider() {
                self.draw_divider(buf, cx, row, cw, d, ui, pal);
                row += DIVIDER_DIR_HEIGHT;
                continue;
            }

            if self.renaming && self.cursor == i {
                // Rename input: prompt (cursor icon, file-panel colors) +
                // modal-styled text.
                let prompt = format!("{} ", ui.cursor);
                buf.put_line(cx, row, cw, "", St::fg_bg(pal.modal_fg, pal.modal_bg));
                let px = buf.put_str(cx, row, &prompt, St::new().fg(pal.cursor).bg(pal.file_panel_bg));
                let view = if self.rename.value().is_empty() {
                    "│New name".to_string()
                } else {
                    self.rename.view(true)
                };
                let _ = buf.put_str(px, row, &view, St::new().fg(pal.modal_fg).bg(pal.modal_bg));
                row += 1;
                continue;
            }

            let cursor = if self.cursor == i && focused && !self.search_focused {
                ui.cursor
            } else {
                " "
            };
            let (fg, bg) = if d.location == current_file_panel_location {
                (pal.sidebar_sel_fg, pal.sidebar_sel_bg)
            } else {
                (pal.sidebar_fg, pal.sidebar_bg)
            };
            let row_st = St::new().fg(fg).bg(bg);
            buf.put_line(cx, row, cw, "", plain);
            let px = buf.put_str(cx, row, &format!("{cursor} "), St::new().fg(pal.cursor).bg(pal.file_panel_bg));
            let icon_part = format!("{} ", d.icon);
            let px = buf.put_str(px, row, &icon_part, row_st);
            let name_max = cw.saturating_sub(2).saturating_sub(str_width(&d.icon) + 1);
            let name = truncate_end(&d.name, name_max, "...");
            let _ = buf.put_str(px, row, &name, row_st);
            row += 1;
        }
    }

    /// 3-line section divider: blank / "{icon} {Name} ─…─" / blank.
    fn draw_divider(
        &self,
        buf: &mut RBuf,
        cx: usize,
        row: usize,
        cw: usize,
        d: &Dir,
        ui: &Ui,
        pal: &Palette,
    ) {
        let plain = St::fg_bg(pal.sidebar_fg, pal.sidebar_bg);
        buf.put_line(cx, row, cw, "", plain);
        let (icon, name) = match d.location.as_str() {
            HOME_DIVIDER => (ui.home, "Home"),
            PINNED_DIVIDER => (ui.pinned, "Pinned"),
            DISK_DIVIDER => (ui.disk, "Disks"),
            _ => return,
        };
        buf.put_line(cx, row + 1, cw, "", plain);
        let px = buf.put_str(
            cx,
            row + 1,
            &format!("{icon}{}{name}", ui.space),
            St::new().fg(pal.sidebar_title).bg(pal.sidebar_bg),
        );
        let _ = buf.put_str(
            px,
            row + 1,
            &format!(" {}", "─".repeat(DIVIDER_LENGTH)),
            St::new().fg(pal.sidebar_divider).bg(pal.sidebar_bg),
        );
        buf.put_line(cx, row + 2, cw, "", plain);
    }
}

// ---------------------------------------------------------------------------
// Free helpers (Go: directory_utils.go / disk_utils.go / pinned.go)
// ---------------------------------------------------------------------------

/// Assemble the final list per the configured section order; dividers are
/// only added between non-empty sections.
fn form_directory_slice(home: &[Dir], pinned: &[Dir], disks: &[Dir], sections: &[String]) -> Vec<Dir> {
    let mut out = Vec::with_capacity(home.len() + pinned.len() + disks.len() + 2);
    for section in sections {
        let (divider, items): (&str, &[Dir]) = match section.as_str() {
            SECTION_HOME => (HOME_DIVIDER, home),
            SECTION_PINNED => (PINNED_DIVIDER, pinned),
            SECTION_DISKS => (DISK_DIVIDER, disks),
            _ => continue,
        };
        if items.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Dir {
                location: divider.to_string(),
                name: String::new(),
                section: "",
                icon: String::new(),
            });
        }
        for d in items {
            out.push(d.clone());
        }
    }
    out
}

/// Load the pinned list, dropping (and re-saving without) entries whose
/// directory no longer exists.
fn load_pinned_clean(path: &Path) -> Vec<PinnedItem> {
    let data = match fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let items: Vec<PinnedItem> = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let cleaned: Vec<PinnedItem> = items
        .iter()
        .filter(|p| fs::metadata(&p.location).is_ok())
        .cloned()
        .collect();
    if cleaned.len() != items.len() {
        let _ = save_pinned(path, &cleaned);
    }
    cleaned
}

fn save_pinned(path: &Path, items: &[PinnedItem]) -> std::io::Result<()> {
    let data = serde_json::to_string(items)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// Go shouldListDisk: "/" or the external-media mount prefixes only.
fn should_list_disk(mp: &str) -> bool {
    if mp == "/" {
        return true;
    }
    if mp.starts_with("/Volumes/.timemachine") {
        return false;
    }
    mp.starts_with("/mnt")
        || mp.starts_with("/media")
        || mp.starts_with("/run/media")
        || mp.starts_with("/Volumes")
}

/// Decode /proc/mounts octal escapes (\040 space, \011 tab, \012 newline).
fn decode_mount_escape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            match &b[i + 1..i + 4] {
                b"040" => out.push(' '),
                b"011" => out.push('\t'),
                b"012" => out.push('\n'),
                _ => {
                    out.push('\\');
                    i += 1;
                    continue;
                }
            }
            i += 4;
        } else {
            let rest = std::str::from_utf8(&b[i..]).unwrap_or(" ");
            let ch = rest.chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Go filepath.Base for display names (keeps "/" for the root).
fn base_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_slice_skips_empty_and_single_divider() {
        let home = vec![Dir {
            location: "/home/u".into(),
            name: "Home".into(),
            section: SECTION_HOME,
            icon: "H".into(),
        }];
        let disks = vec![Dir {
            location: "/".into(),
            name: "Root".into(),
            section: SECTION_DISKS,
            icon: "D".into(),
        }];
        let sections = vec![
            "home".to_string(),
            "pinned".to_string(),
            "disks".to_string(),
        ];
        let out = form_directory_slice(&home, &[], &disks, &sections);
        // Go appendSection: empty sections are skipped, dividers only BETWEEN
        // non-empty sections -> [home item, disk divider, disk item].
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].location, "/home/u");
        assert_eq!(out[1].location, DISK_DIVIDER);
        assert_eq!(out[2].location, "/");
        assert!(out[1].is_divider());
        assert_eq!(out[1].required_height(), 3);
        assert_eq!(out[0].required_height(), 1);
    }

    #[test]
    fn decode_mount_escapes() {
        assert_eq!(decode_mount_escape("/mnt/a\\040b"), "/mnt/a b");
        assert_eq!(decode_mount_escape("/x\\011y\\012z"), "/x\ty\nz");
        assert_eq!(decode_mount_escape("/plain/path"), "/plain/path");
        assert_eq!(decode_mount_escape("/a\\09b"), "/a\\09b");
    }

    #[test]
    fn should_list_disk_rules() {
        assert!(should_list_disk("/"));
        assert!(should_list_disk("/mnt/usb"));
        assert!(should_list_disk("/media/usb"));
        assert!(should_list_disk("/run/media/usb"));
        assert!(should_list_disk("/Volumes/usb"));
        assert!(!should_list_disk("/Volumes/.timemachine/vol"));
        assert!(!should_list_disk("/boot/efi"));
        assert!(!should_list_disk("/tmp"));
    }

    #[test]
    fn base_name_root() {
        assert_eq!(base_name("/"), "/");
        assert_eq!(base_name("/mnt/usb"), "usb");
    }

    #[test]
    fn nav_wraps_and_skips_dividers() {
        let dirs = vec![
            Dir { location: "a".into(), name: "a".into(), section: "home", icon: "".into() },
            Dir { location: HOME_DIVIDER.into(), name: "".into(), section: "", icon: "".into() },
            Dir { location: "b".into(), name: "b".into(), section: "disks", icon: "".into() },
        ];
        // Simulate a minimal sidebar holding these dirs.
        let mut s = make_test_sidebar();
        s.directories = dirs;
        s.cursor = 0;
        s.list_down();
        assert_eq!(s.directories[s.cursor].location, "b");
        s.list_down(); // wraps to top
        assert_eq!(s.directories[s.cursor].location, "a");
        s.list_up(); // wraps to bottom, skips nothing (b is real)
        assert_eq!(s.directories[s.cursor].location, "b");
    }

    fn make_test_sidebar() -> Sidebar {
        Sidebar {
            directories: Vec::new(),
            render_index: 0,
            cursor: 0,
            rename: TextInput::new(),
            renaming: false,
            search: TextInput::new(),
            search_focused: false,
            pinned_file: PathBuf::from("/nonexistent/pinned.json"),
            width: 22,
            height: 20,
            disabled: false,
            sections: vec!["home".into(), "pinned".into(), "disks".into()],
            nerdfont: false,
        }
    }
}
