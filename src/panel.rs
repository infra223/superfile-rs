//! A single file panel: directory listing, cursor, search, sort, selection,
//! rename. Mirrors `src/internal/ui/filepanel` from the Go version.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::fileops::{self, Entry};
use crate::fuzzy;
use crate::text_input::TextInput;
use crate::util;

pub const INNER_PADDING: usize = 4; // cols reserved for truncation (Go: common.InnerPadding)
pub const CONTENT_PADDING: usize = 3; // top bar + search bar + section divider (Go: contentPadding)
pub const MIN_PANEL_WIDTH: u16 = 18;
pub const MIN_PANEL_HEIGHT: u16 = 6; // contentPadding 3 + BorderPadding 2 + 1 (Go MinHeight)
pub const COLUMN_HEADER_HEIGHT: usize = 1;
pub const FILE_SIZE_COLUMN_WIDTH: usize = 15;
pub const MODIFY_TIME_COLUMN_WIDTH: usize = 18;
pub const PERMISSIONS_COLUMN_WIDTH: usize = 12;
pub const COLUMN_DELIMITER: &str = "  ";

/// Re-list throttling (Go: filepanel consts).
pub const NON_FOCUSED_RERENDER_SECS: u64 = 3;
pub const RERENDER_CHUNK_DIVISOR: usize = 100;
pub const RERENDER_MAX_DELAY_SECS: u64 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelMode {
    Browser,
    Select,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKind {
    Name = 0,
    Size = 1,
    Date = 2,
    Type = 3,
    Natural = 4,
}

impl SortKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => SortKind::Size,
            2 => SortKind::Date,
            3 => SortKind::Type,
            4 => SortKind::Natural,
            _ => SortKind::Name,
        }
    }
    pub fn short_label(self) -> &'static str {
        match self {
            SortKind::Name => "Name",
            SortKind::Size => "Size",
            SortKind::Date => "Date",
            SortKind::Type => "Type",
            SortKind::Natural => "Natural",
        }
    }
    pub const ALL: [SortKind; 5] = [
        SortKind::Name,
        SortKind::Size,
        SortKind::Date,
        SortKind::Type,
        SortKind::Natural,
    ];
}

#[derive(Clone)]
pub struct FilePanel {
    pub location: PathBuf,
    pub all_entries: Vec<Entry>,
    pub entries: Vec<Entry>,
    pub loaded: bool,
    pub loading: bool,
    pub cursor: usize,
    pub top: usize,
    pub width: u16,
    pub height: u16,
    pub mode: PanelMode,
    /// Selected item locations (path-based like Go), in selection order.
    pub selected: Vec<PathBuf>,
    pub sort: SortKind,
    pub sort_rev: bool,
    pub sort_case_sensitive: bool,
    /// Live search filter (driven by the search bar value).
    pub search_query: String,
    pub search_active: bool,
    pub search_buf: TextInput,
    pub rename_active: bool,
    pub rename_buf: TextInput,
    pub rename_orig: PathBuf,
    /// Cursor/scroll cache per visited directory (Go: DirectoryRecords).
    pub directory_records: HashMap<PathBuf, (usize, usize)>,
    /// File (by name) to place the cursor on after the next listing loads.
    pub target_file: Option<String>,
    pub last_list: Instant,
    pub dirty: bool,
}

impl FilePanel {
    pub fn new(location: PathBuf, config: &Config) -> Self {
        let location = if location.exists() {
            location
        } else {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        };
        FilePanel {
            location,
            all_entries: Vec::new(),
            entries: Vec::new(),
            loaded: false,
            loading: false,
            cursor: 0,
            top: 0,
            width: 0,
            height: 0,
            mode: PanelMode::Browser,
            selected: Vec::new(),
            sort: SortKind::from_u8(config.default_sort_type),
            sort_rev: config.sort_order_reversed,
            sort_case_sensitive: config.case_sensitive_sort,
            search_query: String::new(),
            search_active: false,
            search_buf: TextInput::new(),
            rename_active: false,
            rename_buf: TextInput::new(),
            rename_orig: PathBuf::new(),
            directory_records: HashMap::new(),
            target_file: None,
            last_list: Instant::now(),
            dirty: true,
        }
    }

    // ----- dimensions -------------------------------------------------

    pub fn content_width(&self) -> usize {
        self.width.saturating_sub(2) as usize
    }

    pub fn content_height(&self) -> usize {
        self.height.saturating_sub(2) as usize
    }

    /// Number of entry rows that fit: content height minus the top bar,
    /// section divider and search bar (3 lines), minus the column header
    /// line when extra columns are enabled. (Go: PanelElementHeight.)
    pub fn element_height(&self, extra_columns: usize) -> usize {
        let header = if extra_columns > 0 { COLUMN_HEADER_HEIGHT } else { 0 };
        self.content_height()
            .saturating_sub(CONTENT_PADDING)
            .saturating_sub(header)
    }

    /// Update width/height clamped to the minimums (Go: SetWidth/SetHeight).
    pub fn set_dimensions(&mut self, width: u16, height: u16) {
        self.width = width.max(MIN_PANEL_WIDTH);
        self.height = height.max(MIN_PANEL_HEIGHT);
        self.search_buf.width = (self.width as usize).saturating_sub(INNER_PADDING);
        self.scroll_to_cursor();
    }

    // ----- listing ----------------------------------------------------

    /// Spawn a directory listing on a worker thread.
    pub fn spawn_list(
        &mut self,
        req_id: u64,
        show_hidden: bool,
        tx: std::sync::mpsc::Sender<crate::event::AsyncMsg>,
    ) {
        let loc = self.location.clone();
        self.loading = true;
        std::thread::spawn(move || {
            let entries = fileops::list_dir(&loc, show_hidden);
            let _ = tx.send(crate::event::AsyncMsg::DirListed {
                req_id,
                path: loc,
                entries,
            });
        });
    }

    /// Apply a finished listing (if still relevant).
    pub fn apply_list(&mut self, path: &Path, entries: Vec<Entry>) {
        if path != self.location {
            return; // stale
        }
        self.all_entries = entries;
        self.loaded = true;
        self.loading = false;
        self.dirty = false;
        self.last_list = Instant::now();
        self.refilter();
        // Jump to target file if requested (Go: applyTargetFileCursor).
        if let Some(name) = self.target_file.take() {
            if let Some(idx) = self.entries.iter().position(|e| e.name == name) {
                self.cursor = idx;
                self.scroll_to_cursor();
            }
        }
    }

    /// Should this panel re-list right now? (Go: shouldSkipPanelUpdate
    /// inverted.) `now` is compared against `last_list`.
    pub fn needs_relist(&self, focused: bool) -> bool {
        if !self.loaded || self.dirty {
            return true;
        }
        let elapsed = self.last_list.elapsed();
        if !focused {
            return elapsed >= Duration::from_secs(NON_FOCUSED_RERENDER_SECS);
        }
        if self.entries.is_empty() {
            return elapsed >= Duration::from_millis(500);
        }
        let delay_secs = (self.entries.len() / RERENDER_CHUNK_DIVISOR)
            .min(RERENDER_MAX_DELAY_SECS as usize) as u64;
        elapsed >= Duration::from_secs(delay_secs)
    }

    /// Re-apply filter + sort to the cached listing.
    pub fn refilter(&mut self) {
        let mut list: Vec<Entry> = if self.search_query.is_empty() {
            self.all_entries.clone()
        } else {
            // fzf-scored, then re-sorted by the chosen sort option
            // (Go: getDirectoryElementsBySearch).
            fuzzy::filter_scored(&self.search_query, &self.all_entries, &|e: &Entry| e.name.clone())
                .into_iter()
                .cloned()
                .collect()
        };
        self.sort_entries(&mut list);
        self.entries = list;
        self.clamp_view();
    }

    /// Go: getOrderingFunc — exact comparators.
    fn less(a: &Entry, b: &Entry, kind: SortKind, reversed: bool, case_sensitive: bool) -> bool {
        // Directories always come first, regardless of reverse order.
        if a.is_dir != b.is_dir {
            return a.is_dir;
        }
        match kind {
            SortKind::Name => {
                let c = if case_sensitive {
                    a.name.cmp(&b.name)
                } else {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                };
                (c == std::cmp::Ordering::Less) != reversed
            }
            SortKind::Natural => {
                let c = if case_sensitive {
                    util::natural_cmp(&a.name, &b.name)
                } else {
                    util::natural_cmp(
                        &a.name.to_ascii_lowercase(),
                        &b.name.to_ascii_lowercase(),
                    )
                };
                (c == std::cmp::Ordering::Less) != reversed
            }
            // For directories, Entry.size holds the child count
            // (Go: compares child count for two directories).
            SortKind::Size => (a.size < b.size) != reversed,
            // Date: no dir-first; newest first by default.
            SortKind::Date => (a.mtime > b.mtime) != reversed,
            SortKind::Type => {
                let ext_cmp = a
                    .ext()
                    .to_ascii_lowercase()
                    .cmp(&b.ext().to_ascii_lowercase());
                if ext_cmp != std::cmp::Ordering::Equal {
                    return ext_cmp == std::cmp::Ordering::Less;
                }
                // Fall back to name, honoring case sensitivity.
                if case_sensitive {
                    a.name.cmp(&b.name) == std::cmp::Ordering::Less
                } else {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                        == std::cmp::Ordering::Less
                }
            }
        }
    }

    fn sort_entries(&self, list: &mut [Entry]) {
        let kind = self.sort;
        let reversed = self.sort_rev;
        let case_sensitive = self.sort_case_sensitive;
        list.sort_unstable_by(|a, b| {
            if Self::less(a, b, kind, reversed, case_sensitive) {
                std::cmp::Ordering::Less
            } else if Self::less(b, a, kind, reversed, case_sensitive) {
                std::cmp::Ordering::Greater
            } else {
                // Deterministic tie-break (Go's unstable sort has none).
                a.name.cmp(&b.name)
            }
        });
    }

    /// Re-read case sensitivity from config (used when config changes).
    pub fn set_case_sensitive(&mut self, v: bool) {
        self.sort_case_sensitive = v;
    }

    // ----- navigation -------------------------------------------------

    pub fn empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// Locations of visible entries, in the order the user sees them.
    /// Falls back to the focused item when nothing is selected.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let visible: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| self.selected.contains(&e.path))
            .map(|e| e.path.clone())
            .collect();
        if visible.is_empty() {
            if let Some(e) = self.cursor_entry() {
                vec![e.path.clone()]
            } else {
                vec![]
            }
        } else {
            visible
        }
    }

    /// Go: scrollToCursor — adjust the top of the viewport around the cursor.
    pub fn scroll_to_cursor(&mut self) {
        if self.cursor >= self.entries.len() {
            return;
        }
        let h = self.element_height(0).max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor > self.top + h - 1 {
            self.top = self.cursor - h + 1;
        }
    }

    fn clamp_view(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.top = 0;
        } else {
            self.cursor = self.cursor.min(self.entries.len() - 1);
            self.top = self.top.min(self.entries.len().saturating_sub(1));
            self.scroll_to_cursor();
        }
        // Drop selections that no longer exist on disk.
        self.selected.retain(|p| p.exists());
    }

    /// Go: moveCursorBy — cursor wraps around.
    fn move_cursor_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len();
        self.cursor = (self.cursor as isize + delta + n as isize).rem_euclid(n as isize) as usize;
        self.scroll_to_cursor();
    }

    /// Go: pageScrollBy — cursor clamped to [0, count-1].
    fn page_scroll_by(&mut self, delta: isize, page_scroll_size: usize) {
        if self.entries.is_empty() {
            return;
        }
        let step = if page_scroll_size <= 0 {
            self.element_height(0).max(1)
        } else {
            page_scroll_size
        };
        let n = self.entries.len() as isize;
        let mut cursor = self.cursor as isize + delta * (step as isize);
        if cursor < 0 {
            cursor = 0;
        } else if cursor >= n {
            cursor = n - 1;
        }
        self.cursor = cursor as usize;
        self.scroll_to_cursor();
    }

    pub fn list_up(&mut self) {
        self.move_cursor_by(-1);
    }

    pub fn list_down(&mut self) {
        self.move_cursor_by(1);
    }

    pub fn page_up(&mut self, page_scroll_size: usize) {
        self.page_scroll_by(-1, page_scroll_size);
    }

    pub fn page_down(&mut self, page_scroll_size: usize) {
        self.page_scroll_by(1, page_scroll_size);
    }

    /// Change directory. Mirrors Go's UpdateCurrentFilePanelDir: resolves
    /// relative paths, no-ops on the same dir, caches cursor/scroll per dir,
    /// validates the target, sets the target file when moving to the parent,
    /// restores cached position and resets the search bar.
    pub fn cd(&mut self, dir: PathBuf) -> Result<(), String> {
        let dir = if dir.is_absolute() {
            dir
        } else {
            self.location.join(&dir)
        };
        if dir == self.location {
            return Ok(());
        }
        // Cache current cursor/scroll in case we switch back.
        self.directory_records
            .insert(self.location.clone(), (self.cursor, self.top));
        match std::fs::metadata(&dir) {
            Err(e) => {
                return Err(format!(
                    "{} : no such file or directory, stats err : {}",
                    dir.display(),
                    e
                ))
            }
            Ok(info) if !info.is_dir() => {
                return Err(format!("{} is not a directory", dir.display()))
            }
            Ok(_) => {}
        }
        // When switching to the parent, explicitly target the dir we left.
        if fileops::parent_of(&self.location) == dir {
            self.target_file = self
                .location
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
        }
        self.location = dir;
        // Restore cached cursor/scroll for this directory (Go: DirectoryRecords).
        let (c, t) = self
            .directory_records
            .get(&self.location)
            .copied()
            .unwrap_or((0, 0));
        self.cursor = c;
        self.top = t;
        // Reset the search bar value (clears the live filter too).
        self.search_query.clear();
        self.search_active = false;
        self.search_buf = TextInput::new();
        self.entries.clear();
        self.all_entries.clear();
        self.loaded = false;
        self.loading = false;
        self.dirty = true;
        Ok(())
    }

    /// Back to parent (Go: ParentDirectory → UpdateCurrentFilePanelDir("..")).
    pub fn parent(&mut self) -> Result<(), String> {
        self.cd(PathBuf::from(".."))
    }

    // ----- selection ---------------------------------------------------

    /// Go: ChangeFilePanelMode — leaving select mode clears the selection.
    pub fn change_mode(&mut self) {
        self.mode = match self.mode {
            PanelMode::Browser => PanelMode::Select,
            PanelMode::Select => {
                self.selected.clear();
                PanelMode::Browser
            }
        };
    }

    pub fn reset_selected(&mut self) {
        self.selected.clear();
    }

    pub fn toggle_selected(&mut self, path: &Path) {
        if let Some(pos) = self.selected.iter().position(|p| p == path) {
            self.selected.remove(pos);
        } else {
            self.selected.push(path.to_path_buf());
        }
    }

    /// Go: SingleItemSelect — toggle the item under the cursor.
    pub fn single_item_select(&mut self) {
        if let Some(e) = self.cursor_entry() {
            let p = e.path.clone();
            self.toggle_selected(&p);
        }
    }

    /// Go: ItemSelectUp — toggle the current item, then move up.
    pub fn item_select_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.single_item_select();
        self.list_up();
    }

    /// Go: ItemSelectDown — toggle the current item, then move down.
    pub fn item_select_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.single_item_select();
        self.list_down();
    }

    pub fn select_all(&mut self) {
        self.selected = self.entries.iter().map(|e| e.path.clone()).collect();
    }

    pub fn is_selected(&self, path: &Path) -> bool {
        self.selected.contains(&path.to_path_buf())
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    // ----- search -------------------------------------------------------

    pub fn focus_search(&mut self) {
        self.search_active = true;
        self.search_buf = TextInput::new();
        // Go: SearchBar.SetWidth(m.width - common.InnerPadding)
        self.search_buf.width = (self.width as usize).saturating_sub(INNER_PADDING);
    }

    /// Esc: blur and clear the value (clears the filter).
    pub fn cancel_search(&mut self) {
        self.search_active = false;
        self.search_buf = TextInput::new();
        self.search_query.clear();
        self.refilter();
    }

    /// Enter: blur only; the filter stays applied (live search).
    pub fn confirm_search(&mut self) {
        self.search_active = false;
    }

    /// Called on every keystroke while the search bar is focused.
    pub fn apply_search_input(&mut self, value: &str) {
        self.search_query = value.to_string();
        self.refilter();
    }

    // ----- rename -------------------------------------------------------

    /// Enter rename mode with the cursor before the extension
    /// (Go: startRename — dirs keep the cursor at the end).
    pub fn start_rename(&mut self) {
        let (path, name, is_dir) = match self.cursor_entry() {
            Some(e) => (e.path.clone(), e.name.clone(), e.is_dir),
            None => return,
        };
        self.rename_orig = path;
        self.rename_active = true;
        self.rename_buf = TextInput::with_value(name.clone());
        if !is_dir {
            if let Some(pos) = name.rfind('.') {
                if pos > 0 {
                    self.rename_buf.cursor = name[..pos].len();
                }
            }
        }
    }

    pub fn cancel_rename(&mut self) {
        self.rename_active = false;
        self.rename_buf = TextInput::new();
    }

    /// Validate the rename; returns the target path.
    pub fn rename_target(&self) -> Option<PathBuf> {
        let name = self.rename_buf.value();
        if name.is_empty() || name.contains('/') {
            return None;
        }
        let parent = self.rename_orig.parent()?.to_path_buf();
        Some(parent.join(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64, mtime_secs: u64) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            is_symlink: false,
            size,
            mtime: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
            mode: 0o100644,
        }
    }

    fn panel_with(entries: Vec<Entry>) -> FilePanel {
        let mut p = FilePanel::new(PathBuf::from("/tmp"), &Config::default());
        p.all_entries = entries;
        p.loaded = true;
        p.width = 40;
        p.height = 20;
        p.refilter();
        p
    }

    #[test]
    fn dirs_first_regardless_of_reverse() {
        let mut p = panel_with(vec![
            entry("b.txt", false, 1, 0),
            entry("a", true, 1, 0),
            entry("c.txt", false, 1, 0),
        ]);
        p.sort_rev = true;
        p.refilter();
        assert_eq!(p.entries[0].name, "a");
        // files reversed: c before b
        assert_eq!(p.entries[1].name, "c.txt");
        assert_eq!(p.entries[2].name, "b.txt");
    }

    #[test]
    fn size_sort_dirs_by_child_count() {
        let mut p = panel_with(vec![
            entry("bigdir", true, 10, 0),
            entry("small.txt", false, 5, 0),
            entry("big.txt", false, 100, 0),
            entry("smalldir", true, 2, 0),
        ]);
        p.sort = SortKind::Size;
        p.sort_rev = false;
        p.refilter();
        // dirs first, by child count: smalldir(2), bigdir(10)
        assert_eq!(p.entries[0].name, "smalldir");
        assert_eq!(p.entries[1].name, "bigdir");
        assert_eq!(p.entries[2].name, "small.txt");
        assert_eq!(p.entries[3].name, "big.txt");
    }

    #[test]
    fn date_sort_newest_first_no_dir_first() {
        let mut p = panel_with(vec![
            entry("old.txt", false, 1, 100),
            entry("newdir", true, 1, 300),
            entry("new.txt", false, 1, 200),
        ]);
        p.sort = SortKind::Date;
        p.sort_rev = false;
        p.refilter();
        assert_eq!(p.entries[0].name, "newdir");
        assert_eq!(p.entries[1].name, "new.txt");
        assert_eq!(p.entries[2].name, "old.txt");
    }

    #[test]
    fn cursor_wraps() {
        let mut p = panel_with(vec![
            entry("a", false, 1, 0),
            entry("b", false, 1, 0),
            entry("c", false, 1, 0),
        ]);
        p.cursor = 0;
        p.list_up();
        assert_eq!(p.cursor, 2);
        p.list_down();
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn type_sort_falls_back_to_name() {
        let mut p = panel_with(vec![
            entry("z.txt", false, 1, 0),
            entry("a.txt", false, 1, 0),
            entry("b.md", false, 1, 0),
        ]);
        p.sort = SortKind::Type;
        p.refilter();
        assert_eq!(p.entries[0].name, "b.md");
        assert_eq!(p.entries[1].name, "a.txt");
        assert_eq!(p.entries[2].name, "z.txt");
    }
}
