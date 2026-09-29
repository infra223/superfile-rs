//! Clipboard footer: copy/cut items with scroll view.

use crate::config::Palette;
use crate::icons::{icon_for, Ui};
use crate::render::{BorderSet, RBuf, St};
use crate::util::truncate_beginning;

use std::fs;
use std::path::Path;

/// Clipboard state: the copy/cut items currently held, the operation
/// mode (copy vs. cut), and the last known footer dimensions.
pub struct Clipboard {
    items: Vec<String>,
    cut: bool,
    // Last known size; draw() receives dimensions per call.
    #[allow(dead_code)]
    width: usize,
    #[allow(dead_code)]
    height: usize,
}

impl Clipboard {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            cut: false,
            width: 0,
            height: 0,
        }
    }

    /// Update the last known footer dimensions.
    pub fn set_dimensions(&mut self, w: usize, h: usize) {
        self.width = w;
        self.height = h;
    }

    /// Drop all items and set the operation mode (true = cut).
    pub fn reset(&mut self, cut: bool) {
        self.items.clear();
        self.cut = cut;
    }

    /// Add a single item to the clipboard.
    pub fn add(&mut self, item: &str) {
        self.items.push(item.to_string());
    }

    /// Replace all items with a copy of `items`.
    pub fn set_items(&mut self, items: &[String]) {
        self.items = items.to_vec();
    }

    /// Whether the held items are marked for cut (vs. copy).
    pub fn is_cut(&self) -> bool {
        self.cut
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// A copy of the current items.
    pub fn items(&self) -> Vec<String> {
        self.items.clone()
    }

    /// Drop items that no longer resolve (e.g. deleted or renamed),
    /// then return a copy of the remaining items.
    pub fn prune_inaccessible_and_get(&mut self) -> Vec<String> {
        self.items.retain(|item| fs::symlink_metadata(item).is_ok());
        self.items.clone()
    }

    /// The first clipboard item, if any.
    pub fn first_item(&self) -> Option<&str> {
        self.items.first().map(|s| s.as_str())
    }

    /// Draw the clipboard footer into `buf` at `(x, y)` with size `w` x `h`.
    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        pal: &Palette,
        ui: &Ui,
    ) {
        // Cover previous frame.
        buf.fill(x, y, w, h, St::fg_bg(pal.footer_bg, pal.footer_bg));
        if w < 2 || h < 2 {
            return;
        }

        // The clipboard never takes focus: plain footer border.
        let border_fg = pal.footer_border;
        buf.border_title(x, y, w, h, BorderSet::plain(), border_fg, pal.footer_bg, "Clipboard", border_fg);

        let cx = x + 1;
        let cw = w.saturating_sub(2);
        let view_h = h.saturating_sub(2);
        let view_w = w.saturating_sub(4);

        if self.items.is_empty() {
            // Blank line, then the "no content" message.
            buf.put_line(cx, y + 1, cw, "", St::fg_bg(pal.footer_fg, pal.footer_bg));
            let none = format!(" {} {} No content in clipboard", ui.error, ui.space);
            buf.put_line(cx, y + 2, cw, &none, St::fg_bg(pal.footer_fg, pal.footer_bg));
            return;
        }

        let len = self.items.len();
        for i in 0..len.min(view_h) {
            let ly = y + 1 + i;
            if i == view_h - 1 && i != len - 1 {
                let left = format!("{} items left....", len - i);
                buf.put_line(cx, ly, cw, &left, St::fg_bg(pal.footer_fg, pal.footer_bg));
                continue;
            }
            let item = &self.items[i];
            let meta = match fs::symlink_metadata(item) {
                Ok(m) => m,
                Err(_) => continue, // skip items that vanished
            };
            let is_dir = meta.file_type().is_dir();
            let is_link = meta.file_type().is_symlink();
            let base = Path::new(item)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.clone());
            let icon = icon_for(&base, is_dir, is_link, ui, None);
            let icon_fg = icon.color.unwrap_or(pal.footer_fg);
            let name = truncate_beginning(item, view_w, "...");
            // Fill the slot background, then draw the icon and the truncated full path.
            buf.put_line(cx, ly, cw, "", St::fg_bg(pal.footer_fg, pal.footer_bg));
            let nx = buf.put_str(cx, ly, &format!("{} ", icon.glyph), St::new().fg(icon_fg).bg(pal.footer_bg));
            buf.put_str(nx, ly, &name, St::new().fg(pal.file_panel_fg).bg(pal.footer_bg));
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}
