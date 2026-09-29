//! Multi-panel group (port of superfile v1.6.0 internal/ui/filemodel) plus
//! file-panel rendering (port of internal/ui/filepanel render/columns).
//!
//! Deviations:
//! (1) the preview panel is not a child of the group — the app draws it;
//!     PanelGroup only computes expected_preview_width (preview_open passed to
//!     set_dimensions, stored for add/close/split);
//! (2) focus is group-level (focused_idx) — no per-panel IsFocused flag;
//! (3) the dot-file display flag is owned by the app, not the group;
//! (4) the initial-path file→parent conversion (Go defaultFilePanel) happens in
//!     PanelGroup::new;
//! (5) column layout is recomputed at draw time instead of cached on SetWidth;
//! (6) rename input width (Go: SetWidth(single_panel_width - 4)) is achieved by
//!     pre-clipping the view after the 2-col prompt;
//! (7) the section divider is drawn with full box coords so the border glyphs on
//!     that row become ├/┤ (Go GetBorder dividerIdx behavior);
//! (8) BorderSet::plain() glyphs everywhere (house convention; theme border chars
//!     not ported);
//! (9) footer-info icon fallback uses Go's AreInfoItemsTruncated condition
//!     ((w-2)/cnt - 3 per item), then the house border_info helper does the layout;
//! (10) the group also stores config.file_preview_width (refreshed by
//!     set_dimensions/add_panel) so that close_focused — whose ported signature
//!     carries no config argument — can re-run the width distribution with the
//!     last known preview width;
//! (11) Hotkeys is imported from crate::config (where the type actually lives),
//!     not crate::keys as the contract sketch suggested.

use std::path::PathBuf;

use ratatui::style::Color;

use crate::config::{Config, Hotkeys, Palette};
use crate::icons::Ui;
use crate::panel::{
    COLUMN_DELIMITER, CONTENT_PADDING, FILE_SIZE_COLUMN_WIDTH, INNER_PADDING,
    MIN_PANEL_HEIGHT, MIN_PANEL_WIDTH, MODIFY_TIME_COLUMN_WIDTH, PanelMode,
    PERMISSIONS_COLUMN_WIDTH, FilePanel,
};
use crate::render::{BorderSet, RBuf, St};

/// Multi-panel group: layout, focus, add/close/split, and rendering
/// (port of Go internal/ui/filemodel + internal/ui/filepanel rendering).
pub struct PanelGroup {
    panels: Vec<FilePanel>,
    focused_idx: usize,
    width: usize,
    height: usize,
    single_panel_width: usize,
    max_panels: usize,
    expected_preview_width: usize,
    preview_open: bool,
    file_preview_width: u8,
}

impl PanelGroup {
    /// Port of filemodel.New + filepanel.FilePanelSlice/defaultFilePanel.
    /// For each path: FilePanel::new(path, config) (which already applies
    /// default sort from config); then the file→parent conversion: if
    /// std::fs::metadata(path) is Ok and !is_dir → panel.location = parent
    /// (or "/" if none), panel.target_file = Some(file_name).
    /// Initial state: focused_idx = 0, width = 18, height = 6,
    /// single_panel_width = 10, max_panels = 0, expected_preview_width = 0,
    /// preview_open = false.
    pub fn new(first_panel_paths: Vec<PathBuf>, config: &Config) -> Self {
        let panels = first_panel_paths
            .iter()
            .map(|path| {
                let mut p = FilePanel::new(path.clone(), config);
                // Go defaultFilePanel: os.Stat follows symlinks; if the path
                // refers to a file, switch to its parent and remember the name.
                if let Ok(meta) = std::fs::metadata(path) {
                    if !meta.is_dir() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            p.target_file = Some(name.to_string());
                        }
                        p.location = path
                            .parent()
                            .map(|d| d.to_path_buf())
                            .unwrap_or_else(|| PathBuf::from("/"));
                    }
                }
                p
            })
            .collect();
        Self {
            panels,
            focused_idx: 0,
            width: MIN_PANEL_WIDTH as usize,
            height: MIN_PANEL_HEIGHT as usize,
            single_panel_width: 10,
            max_panels: 0,
            expected_preview_width: 0,
            preview_open: false,
            file_preview_width: 0,
        }
    }

    /// Port of SetDimensions: clamp w ≥ 18, h ≥ 6, store preview_open,
    /// then update_layout.
    pub fn set_dimensions(&mut self, w: usize, h: usize, preview_open: bool, config: &Config) {
        self.width = w.max(MIN_PANEL_WIDTH as usize);
        self.height = h.max(MIN_PANEL_HEIGHT as usize);
        self.preview_open = preview_open;
        self.file_preview_width = config.file_preview_width;
        self.update_layout();
    }

    pub fn count(&self) -> usize {
        self.panels.len()
    }

    pub fn focused_idx(&self) -> usize {
        self.focused_idx
    }

    /// Only takes effect when i < count (otherwise no-op).
    pub fn set_focused(&mut self, i: usize) {
        if i < self.panels.len() {
            self.focused_idx = i;
        }
    }

    pub fn focus_next(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        self.focused_idx = (self.focused_idx + 1) % self.panels.len();
    }

    pub fn focus_prev(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        let n = self.panels.len();
        self.focused_idx = (self.focused_idx + n - 1) % n;
    }

    pub fn focused_panel(&self) -> &FilePanel {
        &self.panels[self.focused_idx]
    }

    pub fn focused_panel_mut(&mut self) -> &mut FilePanel {
        &mut self.panels[self.focused_idx]
    }

    pub fn panel(&self, i: usize) -> &FilePanel {
        &self.panels[i]
    }

    pub fn panel_mut(&mut self, i: usize) -> &mut FilePanel {
        &mut self.panels[i]
    }

    pub fn panels(&self) -> &[FilePanel] {
        &self.panels
    }

    pub fn panels_mut(&mut self) -> &mut Vec<FilePanel> {
        &mut self.panels
    }

    pub fn max_panels(&self) -> usize {
        self.max_panels
    }

    pub fn single_panel_width(&self) -> usize {
        self.single_panel_width
    }

    pub fn expected_preview_width(&self) -> usize {
        self.expected_preview_width
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Port of CloseFilePanel. Err("minimum panel count reached") when count <= 1.
    /// Removes the focused panel; if the focused index was != 0 it is decremented
    /// (so the PREVIOUS panel becomes focused); re-runs update_layout.
    pub fn close_focused(&mut self) -> Result<(), &'static str> {
        if self.panels.len() <= 1 {
            return Err("minimum panel count reached");
        }
        self.panels.remove(self.focused_idx);
        if self.focused_idx != 0 {
            self.focused_idx -= 1;
        }
        self.update_layout();
        Ok(())
    }

    /// Port of CreateNewFilePanel.
    /// Err("maximum panel count reached") when count >= max_panels;
    /// Err("cannot access location : <loc>") when std::fs::metadata(&location)
    /// fails (Go: os.Stat, follows symlinks).
    /// New panel: FilePanel::new(location, config) then inherit the CURRENTLY
    /// focused panel's sort and sort_rev. Focus moves to the new (last) panel.
    /// Re-runs update_layout with the stored preview_open.
    pub fn add_panel(&mut self, location: PathBuf, config: &Config) -> Result<(), String> {
        if self.panels.len() >= self.max_panels {
            return Err("maximum panel count reached".to_string());
        }
        if std::fs::metadata(&location).is_err() {
            return Err(format!("cannot access location : {}", location.display()));
        }
        let (sort, sort_rev) = {
            let f = &self.panels[self.focused_idx];
            (f.sort, f.sort_rev)
        };
        let mut p = FilePanel::new(location, config);
        p.sort = sort;
        p.sort_rev = sort_rev;
        p.set_dimensions(p.width, self.height as u16);
        self.panels.push(p);
        self.focused_idx = self.panels.len() - 1;
        self.file_preview_width = config.file_preview_width;
        self.update_layout();
        Ok(())
    }

    /// Port of splitPanel: add_panel(focused panel's location, config).
    pub fn split(&mut self, config: &Config) -> Result<(), String> {
        let location = self.panels[self.focused_idx].location.clone();
        self.add_panel(location, config)
    }

    /// Draw all panels side by side starting at (x, y), each at its own
    /// allocated width (sum ≤ group width), same y, height = group height.
    /// Panel i is "focused" iff i == focused_idx. The preview is NOT drawn
    /// here (the app draws it separately).
    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        pal: &Palette,
        ui: &Ui,
        config: &Config,
        hk: &Hotkeys,
    ) {
        let mut px = x;
        for (i, panel) in self.panels.iter().enumerate() {
            Self::draw_panel(buf, px, y, panel, i == self.focused_idx, pal, ui, config, hk);
            px += panel.width as usize;
        }
    }

    /// Exact port of updateChildComponentWidth + updateChildComponentHeight.
    fn update_layout(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        let count = self.panels.len();
        let expected = if self.preview_open {
            if self.file_preview_width == 0 {
                self.width / (count + 1)
            } else {
                self.width / self.file_preview_width as usize
            }
        } else {
            0
        };
        self.expected_preview_width = expected;
        let width_for_panels = self.width.saturating_sub(expected);
        let panel_width = width_for_panels / count;
        let last = width_for_panels - (count - 1) * panel_width;
        for (i, p) in self.panels.iter_mut().enumerate() {
            let pw = if i == count - 1 { last } else { panel_width };
            p.set_dimensions(pw as u16, self.height as u16);
        }
        self.single_panel_width = panel_width;
        self.max_panels = (width_for_panels / MIN_PANEL_WIDTH as usize).min(10);
    }

    /// Port of filepanel.Render.
    fn draw_panel(
        buf: &mut RBuf,
        x: usize,
        y: usize,
        panel: &FilePanel,
        focused: bool,
        pal: &Palette,
        ui: &Ui,
        config: &Config,
        hk: &Hotkeys,
    ) {
        let w = panel.width as usize;
        let h = panel.height as usize;
        let bg = pal.file_panel_bg;
        buf.fill(x, y, w, h, St::fg_bg(bg, bg));
        if w < 2 || h < 2 {
            return;
        }
        let set = BorderSet::plain();
        let border_fg = if focused {
            pal.file_panel_border_active
        } else {
            pal.file_panel_border
        };
        buf.border(x, y, w, h, set, border_fg, bg);

        let cx = x + 1;
        let cw = w - 2;
        let end_x = x + w - 1; // exclusive: last drawable content column is x+w-2

        // Row y+1: top bar (dir icon + truncated path)
        let row = y + 1;
        let seg_a = format!(" {}{}", ui.directory, ui.space);
        let px = draw_str_clipped(
            buf,
            cx,
            row,
            &seg_a,
            St::new().fg(pal.file_panel_top_dir_icon).bg(bg),
            end_x,
        );
        let path_str = crate::util::truncate_beginning(
            &panel.location.display().to_string(),
            cw.saturating_sub(INNER_PADDING),
            "...",
        );
        draw_str_clipped(
            buf,
            px,
            row,
            &path_str,
            St::new().fg(pal.file_panel_top_path).bg(bg),
            end_x,
        );

        // Row y+2: section divider (full box coords → ├/┤ on that row)
        buf.section_divider(x, y + 2, w, set, border_fg, bg);

        // Row y+3: search bar
        let row = y + 3;
        buf.put_line(cx, row, cw, "", St::new().bg(bg));
        let mut px = draw_str_clipped(buf, cx, row, " ", St::new().fg(pal.file_panel_fg).bg(bg), end_x);
        let prompt = format!("{}{}", ui.search, ui.space);
        px = draw_str_clipped(
            buf,
            px,
            row,
            &prompt,
            St::new().fg(pal.file_panel_top_dir_icon).bg(bg),
            end_x,
        );
        let text_st = St::new().fg(pal.file_panel_fg).bg(bg);
        let value = panel.search_buf.value().to_string();
        if value.is_empty() {
            let placeholder = format!(
                "({}) Type something",
                hk.search_bar.first().cloned().unwrap_or_default()
            );
            let s = if panel.search_active {
                format!("│{}", placeholder)
            } else {
                placeholder
            };
            draw_str_clipped(buf, px, row, &s, text_st, end_x);
        } else {
            let s = panel.search_buf.view(panel.search_active);
            draw_str_clipped(buf, px, row, &s, text_st, end_x);
        }

        // Column layout (Go makeColumns), recomputed at draw time.
        let cols = make_columns(cw, config.file_panel_extra_columns, config.file_panel_name_percent);
        let header_present = config.file_panel_extra_columns > 0 && cols.len() > 1;

        let mut row = y + 4;
        if header_present {
            Self::draw_header_row(buf, cx, row, &cols, end_x, pal);
            row += 1;
        }

        // Entry rows (Go renderFileEntries)
        let el_height = panel
            .content_height()
            .saturating_sub(CONTENT_PADDING)
            .saturating_sub(if header_present { 1 } else { 0 });
        if panel.empty() {
            buf.put_line(cx, row, cw, "", St::new().bg(bg));
            let s = format!(" {}{}{}", ui.error, ui.space, "No such file or directory");
            draw_str_clipped(buf, cx, row, &s, St::new().fg(pal.file_panel_fg).bg(bg), end_x);
        } else {
            let end = (panel.top + el_height).min(panel.entries.len());
            for item_idx in panel.top..end {
                let ry = row + (item_idx - panel.top);
                if ry >= y + h - 1 {
                    break;
                }
                if panel.rename_active && item_idx == panel.cursor {
                    Self::draw_rename_row(buf, x, ry, panel, pal, ui, bg);
                } else {
                    Self::draw_entry_row(buf, x, ry, panel, item_idx, &cols, focused, pal, ui, config);
                }
            }
        }

        // Bottom border info (Go renderFooter)
        let c = if panel.empty() { panel.cursor } else { panel.cursor + 1 };
        let cursor_str = format!("{}/{}", c, panel.entries.len());
        let items: Vec<String>;
        if config.show_panel_footer_info {
            let sort_label = panel.sort.short_label().to_string();
            let sort_icon = if panel.sort_rev { ui.sort_desc } else { ui.sort_asc };
            let (mode_label, mode_icon) = match panel.mode {
                PanelMode::Browser => ("Browser".to_string(), ui.browser),
                PanelMode::Select => (
                    format!("Select{}({})", ui.space, panel.selected_count()),
                    ui.select,
                ),
            };
            let sort_l = format!("{} {}", sort_icon, sort_label);
            let mode_l = format!("{} {}", mode_icon, mode_label);
            // Go AreInfoItemsTruncated, 3 items: avail = (w-2)/3 - 3 (saturating).
            let avail = (w.saturating_sub(2) / 3).saturating_sub(3);
            if crate::util::str_width(&sort_l) > avail
                || crate::util::str_width(&mode_l) > avail
                || crate::util::str_width(&cursor_str) > avail
            {
                items = vec![sort_icon.to_string(), mode_icon.to_string(), cursor_str];
            } else {
                items = vec![sort_l, mode_l, cursor_str];
            }
        } else {
            items = vec![cursor_str];
        }
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        buf.border_info(x, y, w, h, set, border_fg, bg, &refs);
    }

    /// Port of renderColumnHeaders.
    fn draw_header_row(buf: &mut RBuf, cx: usize, ry: usize, cols: &[Column], end_x: usize, pal: &Palette) {
        let bg = pal.file_panel_bg;
        let mut px = cx;
        for col in cols {
            buf.put_line(px, ry, col.width, "", St::new().bg(bg));
            let t = crate::util::truncate_end(&col.header, col.width, "...");
            let off = if col.kind == ColKind::Name {
                0
            } else {
                (col.width - crate::util::str_width(&t)) / 2
            };
            draw_str_clipped(buf, px + off, ry, &t, St::new().fg(pal.file_panel_fg).bg(bg), end_x);
            px += col.width;
        }
    }

    /// Port of renderFileEntries per-row rendering over the column definitions.
    fn draw_entry_row(
        buf: &mut RBuf,
        x: usize,
        ry: usize,
        panel: &FilePanel,
        item_idx: usize,
        cols: &[Column],
        focused: bool,
        pal: &Palette,
        ui: &Ui,
        config: &Config,
    ) {
        let entry = &panel.entries[item_idx];
        let selected = panel.is_selected(&entry.path);
        let row_bg = pal.file_panel_bg; // Go forces FilePanelBG on every cell
        let row_fg = if selected { pal.file_panel_sel_fg } else { pal.file_panel_fg };
        let end_x = x + panel.width as usize - 1; // exclusive
        let mut px = x + 1;
        for col in cols {
            buf.put_line(px, ry, col.width, "", St::new().bg(row_bg));
            match col.kind {
                ColKind::Name => {
                    let cursor_part = if item_idx == panel.cursor && !panel.search_active {
                        format!("{} ", ui.cursor)
                    } else {
                        "  ".to_string()
                    };
                    let select_box =
                        if config.show_select_icons && ui.nerdfont && panel.mode == PanelMode::Select {
                            let g = if selected {
                                ui.checkbox_checked
                            } else {
                                ui.checkbox_empty
                            };
                            format!("{}{}", g, ui.space)
                        } else {
                            String::new()
                        };
                    let cursor_w = crate::util::str_width(&cursor_part);
                    let prefix_w = cursor_w + crate::util::str_width(&select_box);
                    // Go wraps cursor+" " in FilePanelCursorStyle even for the
                    // plain-space (non-cursor) case.
                    draw_str_clipped(buf, px, ry, &cursor_part, St::new().fg(pal.cursor).bg(row_bg), end_x);
                    if !select_box.is_empty() {
                        let sb_fg = if focused {
                            pal.file_panel_border_active
                        } else {
                            pal.file_panel_border
                        };
                        draw_str_clipped(
                            buf,
                            px + cursor_w,
                            ry,
                            &select_box,
                            St::new().fg(sb_fg).bg(row_bg),
                            end_x,
                        );
                    }
                    let icon =
                        crate::icons::icon_for(&entry.name, entry.is_dir, entry.is_symlink, ui, pal.directory_icon);
                    let icon_data = format!("{} ", icon.glyph);
                    // Go: filenameWidth <= 0 → whole name cell renders empty.
                    let fname_w = (col.width - prefix_w).saturating_sub(crate::util::str_width(&icon_data));
                    if fname_w > 0 {
                        let ipx = px + prefix_w;
                        let icon_fg = icon.color.unwrap_or(pal.file_panel_fg);
                        draw_str_clipped(
                            buf,
                            ipx,
                            ry,
                            &icon_data,
                            St::new().fg(icon_fg).bg(row_bg),
                            end_x,
                        );
                        let fname = crate::util::truncate_end(&entry.name, fname_w, "...");
                        draw_str_clipped(
                            buf,
                            ipx + crate::util::str_width(&icon_data),
                            ry,
                            &fname,
                            St::new().fg(row_fg).bg(row_bg),
                            end_x,
                        );
                    }
                }
                // Go renders the delimiter in the row colors — visually identical to the bg fill.
                ColKind::Delimiter => {}
                ColKind::Size => {
                    let v = if entry.is_dir {
                        String::new()
                    } else {
                        crate::util::format_file_size(entry.size, config.file_size_use_si)
                    };
                    draw_right_aligned(buf, px, ry, col.width, &v, row_fg, row_bg, end_x);
                }
                ColKind::ModifyTime => {
                    let v = crate::util::format_mtime(entry.mtime);
                    draw_right_aligned(buf, px, ry, col.width, &v, row_fg, row_bg, end_x);
                }
                ColKind::Permission => {
                    let v = crate::metadata::mode_string(entry.mode);
                    draw_right_aligned(buf, px, ry, col.width, &v, row_fg, row_bg, end_x);
                }
            }
            px += col.width;
        }
    }

    /// Port of the rename row: a FOCUSED textinput (Go m.Rename.View()).
    fn draw_rename_row(buf: &mut RBuf, x: usize, ry: usize, panel: &FilePanel, pal: &Palette, ui: &Ui, bg: Color) {
        let cw = panel.width as usize - 2;
        let end_x = x + panel.width as usize - 1; // exclusive
        let px = x + 1;
        buf.put_line(px, ry, cw, "", St::new().bg(bg));
        let prompt = format!("{} ", ui.cursor);
        let qx = draw_str_clipped(buf, px, ry, &prompt, St::new().fg(pal.cursor).bg(bg), end_x);
        // Go GenerateRenameTextInput styles the typed text with ModalStyle.
        let st = St::new().fg(pal.modal_fg).bg(pal.modal_bg);
        let value = panel.rename_buf.value().to_string();
        if value.is_empty() {
            draw_str_clipped(buf, qx, ry, "│New name", st, end_x);
        } else {
            let s = panel.rename_buf.view(true);
            draw_str_clipped(buf, qx, ry, &s, st, end_x);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Name,
    Delimiter,
    Size,
    ModifyTime,
    Permission,
}

#[derive(Debug, Clone)]
struct Column {
    kind: ColKind,
    width: usize,
    header: String,
}

/// Port of makeColumns. content_width = cw.
/// extras = [(Size, "Size", 15), (ModifyTime, "Modify time", 18), (Permission, "Permission", 12)];
/// start with one Name column: header "  Name" (two spaces + "Name"), width = content_width.
/// min_name = content_width * name_percent / 100 (capped at content_width);
/// for the first `extra_columns.min(3)` extras: extra_w = 2 + size;
///   if name_width - extra_w > min_name (saturating; false when the borrow underflows) →
///   push Delimiter(width 2, header "") + the extra col; name_width -= extra_w; else break.
fn make_columns(content_width: usize, extra_columns: usize, name_percent: u8) -> Vec<Column> {
    let extras: [(ColKind, &str, usize); 3] = [
        (ColKind::Size, "Size", FILE_SIZE_COLUMN_WIDTH),
        (ColKind::ModifyTime, "Modify time", MODIFY_TIME_COLUMN_WIDTH),
        (ColKind::Permission, "Permission", PERMISSIONS_COLUMN_WIDTH),
    ];
    let mut name_width = content_width;
    let mut cols = vec![Column {
        kind: ColKind::Name,
        width: name_width,
        header: "  Name".to_string(),
    }];
    let min_name = (content_width * name_percent as usize / 100).min(content_width);
    let delim_w = crate::util::str_width(COLUMN_DELIMITER);
    for (kind, name, size) in extras.iter().take(extra_columns.min(3)) {
        let extra_w = delim_w + size;
        let after = name_width.saturating_sub(extra_w);
        if after > min_name {
            cols.push(Column {
                kind: ColKind::Delimiter,
                width: delim_w,
                header: String::new(),
            });
            cols.push(Column {
                kind: *kind,
                width: *size,
                header: (*name).to_string(),
            });
            name_width = after;
            cols[0].width = name_width;
        } else {
            break;
        }
    }
    cols
}

/// Draw s at (cx, y) in st, pre-clipped so it never passes end_x (exclusive).
/// Returns the new x (== result of put_str, which also clips at the buffer edge).
fn draw_str_clipped(buf: &mut RBuf, cx: usize, y: usize, s: &str, st: St, end_x: usize) -> usize {
    if cx >= end_x {
        return cx;
    }
    let t = crate::util::plain_truncate(s, end_x - cx);
    buf.put_str(cx, y, &t, st)
}

/// Draw v right-aligned inside a `width`-col slot starting at px, pre-clipped at end_x.
fn draw_right_aligned(
    buf: &mut RBuf,
    px: usize,
    ry: usize,
    width: usize,
    v: &str,
    fg: Color,
    bg: Color,
    end_x: usize,
) {
    let v = crate::util::truncate_end(v, width, "...");
    let off = width - crate::util::str_width(&v);
    draw_str_clipped(buf, px + off, ry, &v, St::new().fg(fg).bg(bg), end_x);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileops::Entry;
    use crate::panel::SortKind;
    use std::time::SystemTime;

    /// All-black placeholder palette. The draw tests assert on drawn glyphs
    /// (cell characters), not on colors, so any palette values work.
    fn test_palette() -> Palette {
        Palette {
            file_panel_fg: Color::Black,
            file_panel_bg: Color::Black,
            file_panel_border: Color::Black,
            file_panel_border_active: Color::Black,
            file_panel_top_dir_icon: Color::Black,
            file_panel_top_path: Color::Black,
            file_panel_sel_fg: Color::Black,
            file_panel_sel_bg: Color::Black,
            cursor: Color::Black,
            directory_icon: None,
            cancel: Color::Black,
            correct: Color::Black,
            error: Color::Black,
            footer_bg: Color::Black,
            footer_border: Color::Black,
            footer_border_active: Color::Black,
            footer_fg: Color::Black,
            full_screen_bg: Color::Black,
            full_screen_fg: Color::Black,
            gradient: [None, None],
            help_menu_hotkey: Color::Black,
            help_menu_title: Color::Black,
            hint: Color::Black,
            modal_bg: Color::Black,
            modal_border: Color::Black,
            modal_cancel_bg: Color::Black,
            modal_cancel_fg: Color::Black,
            modal_confirm_bg: Color::Black,
            modal_confirm_fg: Color::Black,
            modal_fg: Color::Black,
            sidebar_bg: Color::Black,
            sidebar_border: Color::Black,
            sidebar_border_active: Color::Black,
            sidebar_divider: Color::Black,
            sidebar_fg: Color::Black,
            sidebar_sel_bg: Color::Black,
            sidebar_sel_fg: Color::Black,
            sidebar_title: Color::Black,
        }
    }

    /// Placeholder icon set with empty glyphs — semantically the non-nerd
    /// icon set (glyphs render as nothing).
    fn test_ui(nerdfont: bool) -> Ui {
        Ui {
            nerdfont,
            space: "",
            browser: "",
            checkbox_checked: "",
            checkbox_empty: "",
            compress_file: "",
            copy: "",
            cursor: "",
            cut: "",
            delete: "",
            desktop: "",
            directory: "",
            disk: "",
            documents: "",
            done: "",
            download: "",
            error: "",
            extract_file: "",
            home: "",
            in_operation: "",
            music: "",
            pictures: "",
            pinned: "",
            public_share: "",
            search: "",
            select: "",
            sort_asc: "",
            sort_desc: "",
            superfile: "",
            templates: "",
            terminal: "",
            trash: "",
            videos: "",
            warn: "",
        }
    }

    fn entry(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            is_symlink: false,
            size,
            mtime: SystemTime::UNIX_EPOCH,
            mode: 0o100644,
        }
    }

    fn panel_with(entries: Vec<Entry>, w: u16, h: u16) -> FilePanel {
        let mut p = FilePanel::new(PathBuf::from("/tmp"), &Config::default());
        p.all_entries = entries;
        p.loaded = true;
        p.width = w;
        p.height = h;
        p.refilter();
        p
    }

    fn group_with(entries: Vec<Entry>, w: usize, h: usize) -> PanelGroup {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.panels_mut().clear();
        g.panels_mut().push(panel_with(entries, w as u16, h as u16));
        g.set_dimensions(w, h, false, &cfg);
        g
    }

    #[test]
    fn make_columns_no_extras() {
        let cols = make_columns(50, 0, 25);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].kind, ColKind::Name);
        assert_eq!(cols[0].width, 50);
        assert_eq!(cols[0].header, "  Name");
    }

    #[test]
    fn make_columns_all_extras_fit() {
        let cols = make_columns(100, 3, 25);
        assert_eq!(cols.len(), 7);
        let kinds: Vec<ColKind> = cols.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ColKind::Name,
                ColKind::Delimiter,
                ColKind::Size,
                ColKind::Delimiter,
                ColKind::ModifyTime,
                ColKind::Delimiter,
                ColKind::Permission
            ]
        );
        assert_eq!(cols[0].width, 49);
        let sum: usize = cols.iter().map(|c| c.width).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn make_columns_narrow_stops_early() {
        // cw=30, percent=25 → min_name=7; Size fits (30-17=13 > 7),
        // Modify time does not (13.saturating_sub(20)=0, not > 7).
        let cols = make_columns(30, 3, 25);
        assert_eq!(cols.len(), 3);
        let kinds: Vec<ColKind> = cols.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![ColKind::Name, ColKind::Delimiter, ColKind::Size]);
        assert_eq!(cols[0].width, 13);
    }

    #[test]
    fn make_columns_full_name_percent() {
        // percent=100 → min_name = cw → no extra can fit.
        let cols = make_columns(60, 3, 100);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].width, 60);
    }

    #[test]
    fn set_dimensions_distributes_width() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp"), PathBuf::from("/")], &cfg);
        g.set_dimensions(41, 20, false, &cfg);
        assert_eq!(g.width(), 41);
        assert_eq!(g.height(), 20);
        assert_eq!(g.panel(0).width, 20);
        assert_eq!(g.panel(1).width, 21);
        assert_eq!(g.single_panel_width(), 20);
        assert_eq!(g.max_panels(), 2);
        assert_eq!(g.expected_preview_width(), 0);
    }

    #[test]
    fn set_dimensions_with_preview() {
        let mut cfg = Config::default();
        cfg.file_preview_width = 0;
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.set_dimensions(40, 20, true, &cfg);
        assert_eq!(g.expected_preview_width(), 20);
        assert_eq!(g.panel(0).width, 20);
        assert_eq!(g.single_panel_width(), 20);
        assert_eq!(g.max_panels(), 1);
    }

    #[test]
    fn add_panel_inherits_sort_and_focus() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.set_dimensions(100, 20, false, &cfg);
        g.focused_panel_mut().sort = SortKind::Size;
        g.focused_panel_mut().sort_rev = true;
        g.add_panel(PathBuf::from("/"), &cfg).unwrap();
        assert_eq!(g.count(), 2);
        assert_eq!(g.focused_idx(), 1);
        assert_eq!(g.panel(1).sort, SortKind::Size);
        assert_eq!(g.panel(1).sort_rev, true);
    }

    #[test]
    fn add_panel_max_reached() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.set_dimensions(18, 20, false, &cfg);
        assert_eq!(g.max_panels(), 1);
        let err = g.add_panel(PathBuf::from("/"), &cfg).unwrap_err();
        assert_eq!(err, "maximum panel count reached");
    }

    #[test]
    fn add_panel_bad_location() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.set_dimensions(100, 20, false, &cfg);
        let err = g.add_panel(PathBuf::from("/nonexistent_dir_xyz_123"), &cfg).unwrap_err();
        assert!(err.contains("cannot access location"), "unexpected err: {err}");
    }

    #[test]
    fn close_focused_min_count() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.set_dimensions(40, 20, false, &cfg);
        assert_eq!(g.close_focused(), Err("minimum panel count reached"));
    }

    #[test]
    fn close_focused_removes_and_refocuses() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp"), PathBuf::from("/")], &cfg);
        g.set_dimensions(60, 20, false, &cfg);
        g.set_focused(1);
        g.close_focused().unwrap();
        assert_eq!(g.count(), 1);
        assert_eq!(g.focused_idx(), 0);
    }

    #[test]
    fn focus_wraps_around() {
        let cfg = Config::default();
        let mut g =
            PanelGroup::new(vec![PathBuf::from("/tmp"), PathBuf::from("/"), PathBuf::from("/")], &cfg);
        g.set_dimensions(60, 20, false, &cfg);
        g.set_focused(2);
        g.focus_next();
        assert_eq!(g.focused_idx(), 0);
        g.focus_prev();
        assert_eq!(g.focused_idx(), 2);
    }

    #[test]
    fn split_uses_focused_location() {
        let cfg = Config::default();
        let mut g = PanelGroup::new(vec![PathBuf::from("/tmp")], &cfg);
        g.set_dimensions(100, 20, false, &cfg);
        g.split(&cfg).unwrap();
        assert_eq!(g.count(), 2);
        assert_eq!(g.focused_idx(), 1);
        assert_eq!(g.panel(1).location, PathBuf::from("/tmp"));
    }

    #[test]
    fn new_file_path_converts_to_parent() {
        let dir = std::env::temp_dir().join("sfr_panels_test_dir_xyz_123");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("somefile.txt");
        std::fs::write(&file, b"x").unwrap();
        let subdir = dir.join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();

        let cfg = Config::default();
        let g = PanelGroup::new(vec![file.clone(), subdir.clone()], &cfg);
        assert_eq!(g.count(), 2);

        let p0 = g.panel(0);
        assert_eq!(p0.location, dir);
        assert_eq!(p0.target_file.as_deref(), Some("somefile.txt"));

        let p1 = g.panel(1);
        assert_eq!(p1.location, subdir);
        assert!(p1.target_file.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn draw_panel_smoke() {
        let cfg = Config::default();
        let pal = test_palette();
        let ui = test_ui(cfg.nerdfont);
        let hk = Hotkeys::default();
        let g = group_with(
            vec![entry("alpha", false, 1234), entry("beta", true, 5)],
            30,
            16,
        );
        let mut buf = RBuf::new(60, 20, Color::Black);
        g.draw(&mut buf, 0, 0, &pal, &ui, &cfg, &hk);

        // border corners
        assert_eq!(buf.cell(0, 0).unwrap().ch, '╭');
        assert_eq!(buf.cell(29, 0).unwrap().ch, '╮');

        // top bar row (y=1) is not blank
        let blank = (1..29).all(|i| buf.cell(i, 1).unwrap().ch == ' ');
        assert!(!blank, "top bar row is blank");

        // first entry row: y+4 (+1 if a header row is present)
        let p = g.panel(0);
        let cols = make_columns(
            p.width as usize - 2,
            cfg.file_panel_extra_columns,
            cfg.file_panel_name_percent,
        );
        let header_present = cfg.file_panel_extra_columns > 0 && cols.len() > 1;
        let ry = 4 + usize::from(header_present);

        // expected column of the name's first char:
        // cx(1) + cursor glyph+" " + select box + icon+" "
        let e0 = &p.entries[0];
        let cursor_part = format!("{} ", ui.cursor); // item 0 is the cursor
        let select_box =
            if cfg.show_select_icons && ui.nerdfont && p.mode == PanelMode::Select {
                let g = if p.is_selected(&e0.path) {
                    ui.checkbox_checked
                } else {
                    ui.checkbox_empty
                };
                format!("{}{}", g, ui.space)
            } else {
                String::new()
            };
        let icon =
            crate::icons::icon_for(&e0.name, e0.is_dir, e0.is_symlink, &ui, pal.directory_icon);
        let icon_data = format!("{} ", icon.glyph);
        let name_x = 1
            + crate::util::str_width(&cursor_part)
            + crate::util::str_width(&select_box)
            + crate::util::str_width(&icon_data);
        let name_x = name_x.min(28);
        let cell = buf.cell(name_x, ry).unwrap();
        assert_eq!(
            cell.ch,
            e0.name.chars().next().unwrap(),
            "entry name not at expected column {name_x} (row {ry})"
        );
    }

    #[test]
    fn draw_empty_panel() {
        let cfg = Config::default();
        let pal = test_palette();
        let ui = test_ui(cfg.nerdfont);
        let hk = Hotkeys::default();
        let g = group_with(vec![], 30, 16);
        let mut buf = RBuf::new(60, 20, Color::Black);
        g.draw(&mut buf, 0, 0, &pal, &ui, &cfg, &hk);

        let p = g.panel(0);
        let cols = make_columns(
            p.width as usize - 2,
            cfg.file_panel_extra_columns,
            cfg.file_panel_name_percent,
        );
        let header_present = cfg.file_panel_extra_columns > 0 && cols.len() > 1;
        let ry = 4 + usize::from(header_present);

        let mut found = false;
        for i in 1..29 {
            if buf.cell(i, ry).unwrap().ch == 'N' {
                found = true;
                break;
            }
        }
        assert!(found, "'No such file or directory' line not found on row {ry}");
    }

    #[test]
    fn footer_info_labels() {
        let mut cfg = Config::default();
        cfg.show_panel_footer_info = true;
        let pal = test_palette();
        let ui = test_ui(cfg.nerdfont);
        let hk = Hotkeys::default();

        // Wide panel (60 cols): full labels fit → bottom border contains "Name".
        let g = group_with(vec![entry("alpha", false, 1)], 60, 16);
        let mut buf = RBuf::new(60, 20, Color::Black);
        g.draw(&mut buf, 0, 0, &pal, &ui, &cfg, &hk);
        let mut bottom = String::new();
        for i in 0..60 {
            bottom.push(buf.cell(i, 15).unwrap().ch);
        }
        assert!(bottom.contains("Name"), "wide footer missing 'Name': {bottom:?}");

        // Narrow panel (18 cols): labels don't fit → icon-only fallback.
        let g = group_with(vec![entry("alpha", false, 1)], 18, 16);
        let mut buf = RBuf::new(60, 20, Color::Black);
        g.draw(&mut buf, 0, 0, &pal, &ui, &cfg, &hk);
        let mut bottom = String::new();
        for i in 0..18 {
            bottom.push(buf.cell(i, 15).unwrap().ch);
        }
        let icon = if g.panel(0).sort_rev {
            ui.sort_desc
        } else {
            ui.sort_asc
        };
        if !icon.is_empty() {
            assert!(bottom.contains(icon), "narrow footer missing sort icon: {bottom:?}");
        }
    }
}
