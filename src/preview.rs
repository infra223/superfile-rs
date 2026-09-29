//! File preview panel and background preview worker.
//!
//! Rust port of the Go version's `src/internal/ui/preview` model plus the
//! `src/pkg/file_preview` rendering pipeline (ANSI half-block image renderer,
//! EXIF orientation, ffmpeg/pdftoppm/gs thumbnails) and
//! `src/pkg/utils/file_utils.go` (`ReadFileContent`, `expandTabs`).
//!
//! The preview box renders, top to bottom in priority:
//! - nothing but the background (panel closed)
//! - "Loading..." (a request is in flight, no content yet)
//! - "Resizing..." (recorded content dims differ from the current box; the
//!   app re-issues the preview command)
//! - message lines (errors / informational, pre-wrapped with the error icon)
//! - text lines (truncated to the content width, no ellipsis)
//! - a directory listing (icon + space + name per entry)
//! - an image rendered with `▄` half-blocks, centered both ways
//!
//! Accepted parity gaps vs. the Go version (no Rust equivalent available):
//! - chroma/ansichroma syntax highlighting and the `bat` code previewer:
//!   text files are always previewed as plain text (the text-file check is
//!   therefore always applied, while Go skips it for chroma-lexed files).
//! - the Kitty image protocol (ANSI half-blocks only).
//! - Go's in-memory preview/thumbnail caches: each request recomputes.
//! - the preview border uses the plain Unicode border set (the draw()
//!   signature has no access to the config's custom border glyphs).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use image::{imageops, ImageFormat, ImageReader, RgbaImage};
use ratatui::style::Color;
use crate::render::BorderSet;

use crate::config::{Config, Palette};
use crate::event::{AsyncMsg, PreviewContent};
use crate::icons::{icon_for, Ui};
use crate::render::{RBuf, St};
use crate::util::{is_text_file, plain_truncate, str_width};

// ---------------------------------------------------------------------------
// Constants (Go: common / file_preview consts)
// ---------------------------------------------------------------------------

const MAX_IMAGE_FILE_SIZE: u64 = 100 * 1024 * 1024; // Go: maxFileSize
const MAX_IMAGE_WIDTH: usize = 1920; // Go: maxImageWidth
const MAX_IMAGE_HEIGHT: usize = 1080; // Go: maxImageHeight
const THUMB_TIMEOUT: Duration = Duration::from_secs(30); // Go: thumbGenerationTimeout
const MAX_SCAN_LINE_BYTES: usize = 64 * 1024; // Go: bufio.Scanner default token limit
const TAB_WIDTH: usize = 4; // Go: utils.TabWidth

const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tiff", ".svg", ".webp", ".ico"];
const VIDEO_EXTENSIONS: &[&str] = &[".mkv", ".mp4", ".mov", ".avi", ".flv", ".webm", ".wmv", ".m4v", ".mpeg", ".3gp", ".ogv"];
const UNSUPPORTED_PREVIEW_FORMATS: &[&str] = &[".torrent"];

// Exact user-facing strings (Go: common.LoadPrerenderedVariables).
const NO_FILE_INFO: &str = "Could not get file info";
const UNSUPPORTED_FILE_MODE: &str = "Unsupported File Mode";
const UNSUPPORTED_FORMATS: &str = "Unsupported formats";
const CANNOT_READ_DIR: &str = "Cannot read directory";
const EMPTY: &str = "Empty";
const ERROR: &str = "Error";
const IMAGE_DISABLED: &str = "Image preview is disabled";
const UNSUPPORTED_IMAGE_FORMATS: &str = "Unsupported image formats";
const IMAGE_CONVERSION_ERROR: &str = "Error converting image to ANSI";
const THUMB_FAILED: &str = "Thumbnail generation failed";

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/// The file preview panel (Go: `ui/preview.Model`).
///
/// The content and the dimensions it was computed for are kept in sync: the
/// box shows "Resizing..." until a preview computed at the current size is
/// applied (or a stale-guarded `apply` arrives).
#[derive(Debug)]
pub struct Preview {
    open: bool,
    path: Option<PathBuf>,
    req_id: u64,
    content: Option<PreviewContent>,
    /// (width, height) of the content area the current content was computed
    /// for.
    content_dims: (usize, usize),
    border_enabled: bool,
    /// Preview box dimensions (border included), from the last layout pass.
    width: u16,
    height: u16,
}

impl Preview {
    /// `border_enabled` comes from `config.enable_file_preview_border`.
    pub fn new(border_enabled: bool) -> Self {
        Self {
            open: false,
            path: None,
            req_id: 0,
            content: None,
            content_dims: (0, 0),
            border_enabled,
            width: 0,
            height: 0,
        }
    }

    /// Preview box dimensions, border included.
    pub fn set_dimensions(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Request a preview of `path` under `req_id`: the panel opens and shows
    /// "Loading..." until the matching `AsyncMsg::Preview` is applied.
    pub fn open_at(&mut self, path: PathBuf, req_id: u64) {
        self.open = true;
        self.path = Some(path);
        self.req_id = req_id;
        self.content = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.path = None;
        self.content = None;
        self.req_id = 0;
    }

    /// Blank box for empty panels (Go: SetEmptyWithDimensions).
    pub fn set_empty(&mut self) {
        self.content = Some(PreviewContent::Message(Vec::new()));
        self.content_dims = self.content_size(self.width as usize, self.height as usize);
    }

    /// Apply a computed preview.
    ///
    /// Stale guard: ignored when `req_id != self.req_id` or `path` is not the
    /// currently requested path. On success the content is stored and the
    /// current content dimensions are recorded.
    pub fn apply(&mut self, req_id: u64, path: &Path, content: PreviewContent) {
        if req_id != self.req_id || self.path.as_deref() != Some(path) {
            return;
        }
        self.content = Some(content);
        self.content_dims = self.content_size(self.width as usize, self.height as usize);
    }

    /// Content area size for a box of (w, h).
    fn content_size(&self, w: usize, h: usize) -> (usize, usize) {
        let pad = if self.border_enabled { 2 } else { 0 };
        (w.saturating_sub(pad), h.saturating_sub(pad))
    }

    /// Render into `buf` at (x, y) with size (w, h) — the preview BOX
    /// (border included). The whole rect is filled with the file panel
    /// background first; nothing is drawn outside it.
    ///
    /// The border is drawn only when enabled, with the PLAIN file panel
    /// border color (Go never uses the active color for this panel);
    /// `focused` is accepted for signature parity but has no effect.
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
    ) {
        let _ = focused;

        // Components must cover their previous frame completely.
        buf.fill(x, y, w, h, St::fg_bg(pal.file_panel_fg, pal.file_panel_bg));

        let bordered = self.border_enabled;
        if bordered {
            buf.border(x, y, w, h, BorderSet::plain(), pal.file_panel_border, pal.file_panel_bg);
        }
        if !self.open {
            return;
        }

        let (cx, cy, cw, ch) = if bordered {
            (x + 1, y + 1, w.saturating_sub(2), h.saturating_sub(2))
        } else {
            (x, y, w, h)
        };
        if cw == 0 || ch == 0 {
            return;
        }

        match &self.content {
            None => {
                buf.put_str(cx, cy, "Loading...", St::new().fg(pal.file_panel_fg));
            }
            Some(content) => {
                if self.content_dims != (cw, ch) {
                    buf.put_str(cx, cy, "Resizing...", St::new().fg(pal.file_panel_fg));
                    return;
                }
                match content {
                    PreviewContent::Message(lines) | PreviewContent::Text(lines) => {
                        for (i, line) in lines.iter().take(ch).enumerate() {
                            buf.put_line(cx, cy + i, cw, &plain_truncate(line, cw), St::new().fg(pal.file_panel_fg));
                        }
                    }
                    PreviewContent::Dir(entries) => {
                        for (i, (name, is_dir, is_link)) in entries.iter().take(ch).enumerate() {
                            draw_dir_line(buf, cx, cy + i, cw, name, *is_dir, *is_link, pal, ui);
                        }
                    }
                    PreviewContent::Image { rows } => draw_image_rows(buf, cx, cy, cw, ch, rows, pal),
                }
            }
        }
    }
}

/// One directory entry line: `icon space name`, icon (and its trailing
/// space) in the icon color, name in the panel foreground, truncated to the
/// content width (Go: renderDirectoryPreview line styling).
fn draw_dir_line(buf: &mut RBuf, x: usize, y: usize, cw: usize, name: &str, is_dir: bool, is_link: bool, pal: &Palette, ui: &Ui) {
    let icon = icon_for(name, is_dir, is_link, ui, pal.directory_icon);
    let icon_st = St::new().fg(icon.color.unwrap_or(pal.file_panel_fg));
    let name_st = St::new().fg(pal.file_panel_fg);
    let mut xx = x;
    xx = buf.put_str(xx, y, icon.glyph, icon_st);
    xx = buf.put_str(xx, y, ui.space, icon_st);
    let avail = cw.saturating_sub(str_width(icon.glyph) + str_width(ui.space));
    buf.put_str(xx, y, &plain_truncate(name, avail), name_st);
}

/// Half-block image, centered horizontally and vertically in the content
/// area (Go: AddStyleModifier(Center/Center). AddLines(imageRender)).
fn draw_image_rows(
    buf: &mut RBuf,
    x: usize,
    y: usize,
    cw: usize,
    ch: usize,
    rows: &Vec<Vec<(Option<[u8; 3]>, Option<[u8; 3]>)>>,
    pal: &Palette,
) {
    let row_w = rows.first().map(|r| r.len()).unwrap_or(0);
    let col_off = if row_w <= cw { (cw - row_w) / 2 } else { 0 };
    let row_cnt = rows.len();
    let row_off = if row_cnt <= ch { (ch - row_cnt) / 2 } else { 0 };

    for (ri, row) in rows.iter().enumerate() {
        if row_off + ri >= ch {
            break;
        }
        let yy = y + row_off + ri;
        for (ci, (lower, upper)) in row.iter().enumerate() {
            if col_off + ci >= cw {
                break;
            }
            let xx = x + col_off + ci;
            // Transparent pixels (None) fall back to the panel background.
            let fg = lower.map(|c| Color::Rgb(c[0], c[1], c[2])).unwrap_or(pal.file_panel_bg);
            let bg = upper.map(|c| Color::Rgb(c[0], c[1], c[2])).unwrap_or(pal.file_panel_bg);
            buf.put_char(xx, yy, '▄', St::fg_bg(fg, bg));
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Runs the preview computation on a spawned thread (Go: the filemodel's
/// preview `tea.Cmd`, calling `Model.RenderWithPath`).
///
/// `box_w` / `box_h` are the PREVIEW BOX dimensions (border included).
/// Sends exactly one `AsyncMsg::Preview` (stale results are dropped by
/// `Preview::apply`).
pub fn spawn_preview(
    req_id: u64,
    path: PathBuf,
    config: &Config,
    ui: &Ui,
    box_w: usize,
    box_h: usize,
    tx: Sender<AsyncMsg>,
) {
    // Only `Copy` snapshots may cross the thread boundary (the Config/Ui
    // references do not outlive this call).
    let border = config.enable_file_preview_border;
    let show_image_preview = config.show_image_preview;
    let err_icon = ui.error;
    let err_space = ui.space;

    std::thread::spawn(move || {
        let content = render_with_path(&path, border, show_image_preview, err_icon, err_space, box_w, box_h);
        let _ = tx.send(AsyncMsg::Preview { req_id, path, content });
    });
}

/// Port of Go `preview.Model.RenderWithPath` (sans Kitty and code
/// highlighting).
fn render_with_path(
    path: &Path,
    border: bool,
    show_image_preview: bool,
    err_icon: &str,
    err_space: &str,
    box_w: usize,
    box_h: usize,
) -> PreviewContent {
    // Go: contentWidth = previewWidth - BorderPadding (when the border is
    // enabled); guard against zero-size boxes.
    let pad = usize::from(border) * 2;
    let content_w = box_w.saturating_sub(pad).max(1);
    let content_h = box_h.saturating_sub(pad).max(1);

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return err_lines(err_icon, err_space, NO_FILE_INFO),
    };

    // Non-regular files which are not directories: don't try to read them
    // (Go issue #876).
    if !meta.is_file() && !meta.is_dir() {
        return err_lines(err_icon, err_space, UNSUPPORTED_FILE_MODE);
    }

    let ext = ext_with_dot(path);
    if UNSUPPORTED_PREVIEW_FORMATS.contains(&ext.as_str()) {
        return err_lines(err_icon, err_space, UNSUPPORTED_FORMATS);
    }

    if meta.is_dir() {
        return render_directory(path, err_icon, err_space);
    }

    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return render_image(path, show_image_preview, content_w, content_h, err_icon, err_space);
    }

    // Thumbnails (Go: ThumbnailGenerator with pdf / ps / video generators).
    let thumb = if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        which("ffmpeg").map(|_| ThumbKind::Video)
    } else if ext == ".pdf" {
        which("pdftoppm").map(|_| ThumbKind::Pdf)
    } else if ext == ".ps" || ext == ".eps" {
        which("gs").map(|_| ThumbKind::Ps)
    } else {
        None
    };
    if let Some(kind) = thumb {
        let tmp = new_temp_dir();
        let result = match &tmp {
            Some(dir) => generate_thumbnail(kind, path, dir),
            None => Err("could not create temp directory".to_string()),
        };
        match result {
            Ok(jpg) => {
                let content = render_image(&jpg, show_image_preview, content_w, content_h, err_icon, err_space);
                if let Some(dir) = tmp {
                    let _ = std::fs::remove_dir_all(dir);
                }
                return content;
            }
            Err(detail) => {
                if let Some(dir) = tmp {
                    let _ = std::fs::remove_dir_all(dir);
                }
                return err_lines_detail(err_icon, err_space, THUMB_FAILED, &detail);
            }
        }
    }

    // Text path. (Go skips this check for chroma-lexed files; without chroma
    // it always applies.)
    if !is_text_file(path) {
        return err_lines(err_icon, err_space, UNSUPPORTED_FORMATS);
    }
    read_file_content(path, content_w, content_h, err_icon, err_space)
}

/// Lowercased extension WITH the leading dot (Go: strings.ToLower(filepath.Ext)).
fn ext_with_dot(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

/// Go: wrapFilePreviewErrorMsg(msg) → "\n--- <icon><space>msg ---" as lines.
fn err_lines(icon: &str, space: &str, msg: &str) -> PreviewContent {
    PreviewContent::Message(vec![String::new(), format!("--- {icon}{space}{msg} ---")])
}

/// Error line plus one detail line (Go: renderPreviewError / wrapped errors).
fn err_lines_detail(icon: &str, space: &str, msg: &str, detail: &str) -> PreviewContent {
    PreviewContent::Message(vec![
        String::new(),
        format!("--- {icon}{space}{msg} ---"),
        detail.to_string(),
    ])
}

/// Go: renderDirectoryPreview (full listing; the renderer clips to height).
fn render_directory(path: &Path, err_icon: &str, err_space: &str) -> PreviewContent {
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return err_lines(err_icon, err_space, CANNOT_READ_DIR),
    };
    let mut items: Vec<(String, bool, bool)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // lstat semantics (Go: os.ReadDir entries + DirEntry.Info).
        let ft = entry.file_type().ok();
        let is_dir = ft.map(|t| t.is_dir()).unwrap_or(false);
        let is_link = ft.map(|t| t.is_symlink()).unwrap_or(false);
        items.push((name, is_dir, is_link));
    }
    // Directories first, then name ascending (byte-wise, Go string sort).
    items.sort_by(|a, b| {
        if a.1 != b.1 {
            return b.1.cmp(&a.1);
        }
        a.0.cmp(&b.0)
    });
    if items.is_empty() {
        return err_lines(err_icon, err_space, EMPTY);
    }
    PreviewContent::Dir(items)
}

// ---------------------------------------------------------------------------
// Image pipeline (Go: file_preview prepareImageForPreview + ANSIRenderer)
// ---------------------------------------------------------------------------

/// Go: renderImagePreview + ImagePreviewWithRenderer + ANSIRenderer.
fn render_image(
    path: &Path,
    show_image_preview: bool,
    content_w: usize,
    content_h: usize,
    err_icon: &str,
    err_space: &str,
) -> PreviewContent {
    if !show_image_preview {
        return err_lines(err_icon, err_space, IMAGE_DISABLED);
    }
    // Go: maxFileSize guard (non-format error → conversion error line).
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return err_lines(err_icon, err_space, IMAGE_CONVERSION_ERROR),
    };
    if meta.len() > MAX_IMAGE_FILE_SIZE {
        return err_lines(err_icon, err_space, IMAGE_CONVERSION_ERROR);
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return err_lines(err_icon, err_space, IMAGE_CONVERSION_ERROR),
    };
    match decode_and_render(&data, content_w, content_h) {
        Ok(rows) => PreviewContent::Image { rows },
        // Go: errors.Is(err, image.ErrFormat) → "Unsupported image formats".
        Err(DecodeError::Unsupported) => err_lines(err_icon, err_space, UNSUPPORTED_IMAGE_FORMATS),
        Err(DecodeError::Other(detail)) => err_lines_detail(err_icon, err_space, IMAGE_CONVERSION_ERROR, &detail),
    }
}

enum DecodeError {
    /// No registered decoder matched the file (Go: image.ErrFormat).
    Unsupported,
    /// Decoder failed on a recognized format.
    Other(String),
}

/// Go: prepareImageForPreview (decode + EXIF orientation + 1080p cap) and
/// resizeForANSI (fit to content_w × content_h*2).
fn decode_and_render(
    data: &[u8],
    content_w: usize,
    content_h: usize,
) -> Result<Vec<Vec<(Option<[u8; 3]>, Option<[u8; 3]>)>>, DecodeError> {
    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| DecodeError::Unsupported)?;
    let format = reader.format().ok_or(DecodeError::Unsupported)?;
    let img: RgbaImage = reader
        .decode()
        .map_err(|e| DecodeError::Other(e.to_string()))?
        .to_rgba8();

    // EXIF orientation (JPEGs only; the Go exif crate fails on other formats).
    let img = if format == ImageFormat::Jpeg {
        match exif_orientation(data) {
            Some(o) => apply_orientation(img, o),
            None => {
                eprintln!("superfile: exif orientation unavailable, skipping rotation");
                img
            }
        }
    } else {
        img
    };

    // Cap resolution at 1080p (Go: limitImageResolution).
    let img = if img.width() as usize > MAX_IMAGE_WIDTH || img.height() as usize > MAX_IMAGE_HEIGHT {
        fit_image(&img, MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT)
    } else {
        img
    };

    // Fit to the preview; height is doubled because each terminal row holds
    // two pixel rows (Go: resizeForANSI with heightScaleFactor = 2).
    let img = fit_image(&img, content_w, content_h * 2);

    Ok(to_half_block_rows(&img))
}

/// Go: adjustOrientation (imaging package; rotate90 = 90° clockwise).
fn apply_orientation(img: RgbaImage, orientation: u16) -> RgbaImage {
    match orientation {
        2 => imageops::flip_horizontal(&img),
        3 => imageops::rotate180(&img),
        4 => imageops::flip_vertical(&img),
        5 => transpose(&img),
        6 => imageops::rotate270(&img),
        7 => transverse(&img),
        8 => imageops::rotate90(&img),
        // 1 (or unknown): unchanged. Go: slog.Error("Invalid orientation
        // value") for unknown values, image unchanged either way.
        _ => img,
    }
}

/// imaging.Transpose: new(x, y) = old(y, x); dimensions swap.
/// `out` is h x w, so x runs 0..h and y runs 0..w (the SOURCE's axes).
fn transpose(img: &RgbaImage) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let mut out = RgbaImage::new(h, w);
    for y in 0..w {
        for x in 0..h {
            out.put_pixel(x, y, *img.get_pixel(y, x));
        }
    }
    out
}

/// imaging.Transverse: flip about the anti-diagonal,
/// new(x, y) = old(w-1-y, h-1-x); dimensions swap.
fn transverse(img: &RgbaImage) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let mut out = RgbaImage::new(h, w);
    for y in 0..w {
        for x in 0..h {
            out.put_pixel(x, y, *img.get_pixel(w - 1 - y, h - 1 - x));
        }
    }
    out
}

/// imaging.Fit with a Lanczos filter: shrink to fit (max_w, max_h) keeping
/// the aspect ratio; images that already fit are returned unchanged.
fn fit_image(img: &RgbaImage, max_w: usize, max_h: usize) -> RgbaImage {
    let w = img.width() as f64;
    let h = img.height() as f64;
    if w == 0.0 || h == 0.0 {
        return img.clone();
    }
    let scale = (max_w as f64 / w).min(max_h as f64 / h);
    if scale >= 1.0 {
        return img.clone();
    }
    let nw = (w * scale).round().max(1.0) as u32;
    let nh = (h * scale).round().max(1.0) as u32;
    imageops::resize(img, nw, nh, imageops::FilterType::Lanczos3)
}

/// Go: ConvertImageToANSI — one `▄` row per two pixel rows; a pixel with
/// alpha 0 becomes `None` (the renderer substitutes the panel background).
fn to_half_block_rows(img: &RgbaImage) -> Vec<Vec<(Option<[u8; 3]>, Option<[u8; 3]>)>> {
    let (w, h) = (img.width(), img.height());
    let mut rows = Vec::with_capacity((h as usize + 1) / 2);
    for y in 0..h {
        if y % 2 != 0 {
            continue;
        }
        let mut row = Vec::with_capacity(w as usize);
        for x in 0..w {
            let upper = pixel_rgb(img, x, y);
            let lower = if y + 1 < h { pixel_rgb(img, x, y + 1) } else { None };
            row.push((lower, upper));
        }
        rows.push(row);
    }
    rows
}

fn pixel_rgb(img: &RgbaImage, x: u32, y: u32) -> Option<[u8; 3]> {
    let p = img.get_pixel(x, y);
    (p.0[3] != 0).then_some([p.0[0], p.0[1], p.0[2]])
}

// ---------------------------------------------------------------------------
// EXIF orientation (minimal manual JPEG/TIFF parse)
// ---------------------------------------------------------------------------

/// Scan the JPEG segment stream for an APP1 marker containing the
/// `Exif\0\0` payload and read the IFD0 Orientation tag (0x0112, SHORT).
/// Returns `None` on any failure (→ no rotation).
fn exif_orientation(data: &[u8]) -> Option<u16> {
    let mut i = 0usize;
    while i + 2 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        match data[i + 1] {
            0xD8 => i += 2, // SOI
            0xD9 => return None, // EOI
            0xDA => return None, // SOS: entropy-coded data follows
            0x01 | 0xD0..=0xD7 => i += 2, // stand-alone markers
            marker => {
                if i + 4 > data.len() {
                    return None;
                }
                let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
                if len < 2 {
                    return None;
                }
                let payload = data.get(i + 4..(i + 2 + len).min(data.len()))?;
                if marker == 0xE1 {
                    if let Some(pos) = payload.windows(6).position(|w| w == b"Exif\0\0") {
                        return tiff_orientation(&payload[pos + 6..]);
                    }
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// Parse a TIFF header's first IFD and return the Orientation value.
fn tiff_orientation(b: &[u8]) -> Option<u16> {
    if b.len() < 8 {
        return None;
    }
    let little = b[0] == b'I' && b[1] == b'I';
    let big = b[0] == b'M' && b[1] == b'M';
    if !little && !big {
        return None;
    }
    let r16 = |off: usize| -> Option<u16> {
        let p = b.get(off..off + 2)?;
        if little {
            Some(u16::from_le_bytes([p[0], p[1]]))
        } else {
            Some(u16::from_be_bytes([p[0], p[1]]))
        }
    };
    let r32 = |off: usize| -> Option<u32> {
        let p = b.get(off..off + 4)?;
        if little {
            Some(u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
        } else {
            Some(u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
        }
    };
    if r16(2)? != 42 {
        return None;
    }
    let ifd = r32(4)? as usize;
    let count = r16(ifd)? as usize;
    for k in 0..count {
        let off = ifd + 2 + k * 12;
        let tag = r16(off)?;
        let typ = r16(off + 2)?;
        if tag == 0x0112 && typ == 3 {
            let v = b.get(off + 8..off + 10)?;
            return if little {
                Some(u16::from_le_bytes([v[0], v[1]]))
            } else {
                Some(u16::from_be_bytes([v[0], v[1]]))
            };
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Thumbnails (Go: file_preview.thumbnail_generator)
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum ThumbKind {
    Video,
    Pdf,
    Ps,
}

/// Go: exec.LookPath — first executable file named `name` on $PATH.
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Fresh temp dir, Go-style `superfiles-*` naming; pid + counter instead of
/// the Go runtime's random hex.
fn new_temp_dir() -> Option<PathBuf> {
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("superfiles-{n:x}-{:x}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Go: the three generateThumbnail implementations, with a 30s timeout.
fn generate_thumbnail(kind: ThumbKind, input: &Path, tmp: &Path) -> Result<PathBuf, String> {
    // Go: baseName = filename minus its extension.
    let base = input.file_stem().and_then(|s| s.to_str()).unwrap_or("thumbnail");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prefix = tmp.join(format!("{base}-{nanos}"));
    let out = prefix.with_extension("jpg");
    let out_str = out.to_string_lossy().into_owned();
    let prefix_str = prefix.to_string_lossy().into_owned();
    let gs_out = format!("-sOutputFile={out_str}");

    let mut cmd = match kind {
        ThumbKind::Video => {
            let mut c = Command::new("ffmpeg");
            c.args(["-v", "warning", "-an", "-sn", "-dn", "-t", "180", "-hwaccel", "auto", "-skip_frame", "nokey", "-i"])
                .arg(input)
                .args(["-vf", "thumbnail", "-frames:v", "1", "-f", "image2", "-fs", "104857600", "-y"])
                .arg(&out);
            c
        }
        ThumbKind::Pdf => {
            let mut c = Command::new("pdftoppm");
            c.args(["-singlefile", "-jpeg"]).arg(input).arg(&prefix_str);
            c
        }
        ThumbKind::Ps => {
            let mut c = Command::new("gs");
            c.args(["-dSAFER", "-dBATCH", "-dNOPAUSE", "-sPageList=1", "-sDEVICE=jpeg", "-r150"])
                .arg(&gs_out)
                .arg(input);
            c
        }
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    run_with_timeout(&mut cmd, THUMB_TIMEOUT)?;

    if out.exists() {
        return Ok(out);
    }
    // Older poppler releases append a page-number suffix even with
    // -singlefile (e.g. "<prefix>-1.jpg"); look for the produced .jpg.
    if kind == ThumbKind::Pdf {
        if let Some(found) = std::fs::read_dir(tmp)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().map(|x| x == "jpg").unwrap_or(false))
        {
            return Ok(found);
        }
    }
    Err("no output image produced".to_string())
}

/// Spawn and wait with a deadline (Go: context.WithTimeout(30s) + kill).
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                if status.success() {
                    return Ok(());
                }
                return Err(match status.code() {
                    Some(code) => format!("exit status {code}"),
                    None => "killed by signal".to_string(),
                });
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("command timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Text preview (Go: utils.ReadFileContent + expandTabs)
// ---------------------------------------------------------------------------

/// Port of Go `utils.ReadFileContent` (maxLineLength = content width,
/// previewLine = content height), lossy-UTF-8 with BOM stripping, tab
/// expansion, display-width truncation, 64KB line limit, and an early stop
/// after `preview_lines` lines.
fn read_file_content(
    path: &Path,
    max_line_length: usize,
    preview_lines: usize,
    err_icon: &str,
    err_space: &str,
) -> PreviewContent {
    use std::io::{BufRead, Read as _, Seek, SeekFrom};

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return err_lines_detail(err_icon, err_space, ERROR, &e.to_string()),
    };
    let mut reader = std::io::BufReader::new(file);

    // Go: unicode.BOMOverride(UTF8.NewDecoder()) — strip a UTF-8/16/32 BOM,
    // decode the rest as (lossy) UTF-8.
    let mut head = [0u8; 4];
    let head_len = match reader.read(&mut head) {
        Ok(n) => n,
        Err(e) => return err_lines_detail(err_icon, err_space, ERROR, &e.to_string()),
    };
    let bom = bom_len(&head[..head_len]);
    if bom > 0 {
        if let Err(e) = reader.seek(SeekFrom::Start(bom as u64)) {
            return err_lines_detail(err_icon, err_space, ERROR, &e.to_string());
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) => return err_lines_detail(err_icon, err_space, ERROR, &e.to_string()),
        };
        if n == 0 {
            break;
        }
        // Go's ScanLines: one mandatory '\n', one optional '\r' before it.
        if buf.ends_with(b"\n") {
            buf.pop();
        }
        if buf.ends_with(b"\r") {
            buf.pop();
        }
        // Go's bufio.Scanner 64KB token limit.
        if buf.len() > MAX_SCAN_LINE_BYTES {
            return err_lines_detail(err_icon, err_space, ERROR, "bufio.Scanner: token too long");
        }
        // Go: expand tabs BEFORE truncation so columns line up.
        let line = String::from_utf8_lossy(&buf);
        lines.push(plain_truncate(&expand_tabs(&line), max_line_length));
        if preview_lines > 0 && lines.len() >= preview_lines {
            break;
        }
    }

    if lines.is_empty() {
        return err_lines(err_icon, err_space, EMPTY);
    }
    PreviewContent::Text(lines)
}

/// Length of the UTF-8/UTF-16/UTF-32 BOM at the start of `head` (0 when
/// there is none).
fn bom_len(head: &[u8]) -> usize {
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else if head.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) || head.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        4
    } else if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        2
    } else {
        0
    }
}

/// Go: utils.expandTabs — replace tabs with spaces up to the next
/// multiple-of-4 DISPLAY-width stop (a tab at a stop still advances 4).
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut sb = String::new();
    let mut last_segment_start = 0usize;
    for r in line.chars() {
        if r == '\t' {
            let seg = &sb[last_segment_start..];
            sb.push_str(&" ".repeat(TAB_WIDTH - str_width(seg) % TAB_WIDTH));
            last_segment_start = sb.len();
            continue;
        }
        sb.push(r);
    }
    sb
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 3] = [255, 0, 0];
    const B: [u8; 3] = [0, 255, 0];
    const C: [u8; 3] = [0, 0, 255];
    const D: [u8; 3] = [255, 255, 0];

    /// 2x2 image: [A B / C D]
    fn sample() -> RgbaImage {
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([A[0], A[1], A[2], 255]));
        img.put_pixel(1, 0, image::Rgba([B[0], B[1], B[2], 255]));
        img.put_pixel(0, 1, image::Rgba([C[0], C[1], C[2], 255]));
        img.put_pixel(1, 1, image::Rgba([D[0], D[1], D[2], 255]));
        img
    }

    fn px(img: &RgbaImage, x: u32, y: u32) -> [u8; 3] {
        let p = img.get_pixel(x, y);
        [p.0[0], p.0[1], p.0[2]]
    }

    #[test]
    fn expand_tabs_basic() {
        assert_eq!(expand_tabs("ab"), "ab");
        assert_eq!(expand_tabs("\t"), "    ");
        assert_eq!(expand_tabs("ab\tc"), "ab  c");
        assert_eq!(expand_tabs("abcd\tc"), "abcd    c");
        assert_eq!(expand_tabs("a\t\t"), "a       ");
    }

    #[test]
    fn expand_tabs_wide_chars() {
        // 'あ' is 2 columns wide: tab advances to column 4.
        assert_eq!(expand_tabs("あ\tb"), "あ  b");
    }

    #[test]
    fn bom_len_variants() {
        assert_eq!(bom_len(&[0xEF, 0xBB, 0xBF, 0x61]), 3);
        assert_eq!(bom_len(&[0xFF, 0xFE, 0x00, 0x00]), 4);
        assert_eq!(bom_len(&[0x00, 0x00, 0xFE, 0xFF]), 4);
        assert_eq!(bom_len(&[0xFF, 0xFE, 0x41]), 2);
        assert_eq!(bom_len(&[0xFE, 0xFF, 0x41]), 2);
        assert_eq!(bom_len(b"abc"), 0);
        assert_eq!(bom_len(&[0xEF, 0xBB]), 0);
        assert_eq!(bom_len(&[]), 0);
    }

    #[test]
    fn exif_orientation_transforms() {
        let o = |k: u16| apply_orientation(sample(), k);
        let four = |img: &RgbaImage| (px(img, 0, 0), px(img, 1, 0), px(img, 0, 1), px(img, 1, 1));
        assert_eq!(four(&o(1)), (A, B, C, D)); // identity
        assert_eq!(four(&o(2)), (B, A, D, C)); // flip horizontal
        assert_eq!(four(&o(3)), (D, C, B, A)); // 180°
        assert_eq!(four(&o(4)), (C, D, A, B)); // flip vertical
        assert_eq!(four(&o(5)), (A, C, B, D)); // transpose
        assert_eq!(four(&o(6)), (B, D, A, C)); // 270° CW (90° CCW)
        assert_eq!(four(&o(7)), (D, B, C, A)); // transverse (anti-diagonal flip)
        assert_eq!(four(&o(8)), (C, A, D, B)); // 90° CW
        assert_eq!(four(&o(9)), (A, B, C, D)); // unknown → unchanged
    }

    #[test]
    fn transpose_and_transverse_swap_dimensions() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        img.put_pixel(1, 0, image::Rgba([4, 5, 6, 255]));
        let t = transpose(&img);
        let v = transverse(&img);
        assert_eq!((t.width(), t.height()), (1, 2));
        assert_eq!((v.width(), v.height()), (1, 2));
        // transpose: new(x, y) = old(y, x)
        assert_eq!(px(&t, 0, 0), [1, 2, 3]);
        assert_eq!(px(&t, 0, 1), [4, 5, 6]);
        // transverse: new(x, y) = old(h-1-y, w-1-x)
        assert_eq!(px(&v, 0, 0), [4, 5, 6]);
        assert_eq!(px(&v, 0, 1), [1, 2, 3]);
    }

    fn jpeg_with_orientation(orientation: u16, little: bool) -> Vec<u8> {
        let p16 = |v: u16| if little { v.to_le_bytes() } else { v.to_be_bytes() };
        let p32 = |v: u32| if little { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut t = vec![b'E', b'x', b'i', b'f', 0, 0];
        t.extend_from_slice(if little { b"II" } else { b"MM" });
        t.extend_from_slice(&p16(42));
        t.extend_from_slice(&p32(8)); // IFD0 at offset 8
        t.extend_from_slice(&p16(1)); // one entry
        t.extend_from_slice(&p16(0x0112)); // Orientation
        t.extend_from_slice(&p16(3)); // SHORT
        t.extend_from_slice(&p32(1)); // count
        t.extend_from_slice(&p16(orientation));
        t.extend_from_slice(&p16(0));

        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
        v.extend_from_slice(&((t.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(&t);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn exif_orientation_parses() {
        assert_eq!(exif_orientation(&jpeg_with_orientation(6, true)), Some(6));
        assert_eq!(exif_orientation(&jpeg_with_orientation(7, false)), Some(7));
        assert_eq!(exif_orientation(b"not a jpeg"), None);
        assert_eq!(exif_orientation(&[0xFF, 0xD8, 0xFF, 0xE1]), None);
    }

    #[test]
    fn half_block_rows_pair_pixels() {
        let img = sample();
        let rows = to_half_block_rows(&img);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[0][0], (Some(C), Some(A)));
        assert_eq!(rows[0][1], (Some(D), Some(B)));
    }

    #[test]
    fn half_block_rows_drop_transparent_and_odd_height() {
        let mut img = RgbaImage::new(1, 2);
        img.put_pixel(0, 0, image::Rgba([9, 8, 7, 255]));
        img.put_pixel(0, 1, image::Rgba([1, 2, 3, 0])); // transparent
        let rows = to_half_block_rows(&img);
        assert_eq!(rows[0], vec![(None, Some([9, 8, 7]))]);

        // Odd height: the last (upper) pixel has no lower half.
        let mut odd = RgbaImage::new(1, 3);
        odd.put_pixel(0, 0, image::Rgba([1, 1, 1, 255]));
        odd.put_pixel(0, 1, image::Rgba([2, 2, 2, 255]));
        odd.put_pixel(0, 2, image::Rgba([3, 3, 3, 255]));
        let rows = to_half_block_rows(&odd);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![(Some([2, 2, 2]), Some([1, 1, 1]))]);
        assert_eq!(rows[1], vec![(None, Some([3, 3, 3]))]);
    }

    #[test]
    fn fit_image_only_shrinks() {
        let img = RgbaImage::new(4, 2);
        let big = fit_image(&img, 8, 8);
        assert_eq!((big.width(), big.height()), (4, 2));
        let small = fit_image(&img, 2, 4);
        assert_eq!((small.width(), small.height()), (2, 1));
    }

    #[test]
    fn error_lines_format() {
        match err_lines("✗", " ", EMPTY) {
            PreviewContent::Message(lines) => {
                assert_eq!(lines, vec!["".to_string(), "--- ✗ Empty ---".to_string()]);
            }
            other => panic!("expected Message, got {other:?}"),
        }
        match err_lines_detail("✗", " ", ERROR, "boom") {
            PreviewContent::Message(lines) => {
                assert_eq!(lines, vec!["".to_string(), "--- ✗ Error ---".to_string(), "boom".to_string()]);
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    fn test_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("spf-preview-test-{n:x}-{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn directory_sorted_dirs_first() {
        let dir = test_dir();
        std::fs::create_dir_all(dir.join("bdir")).unwrap();
        std::fs::create_dir_all(dir.join("adir")).unwrap();
        std::fs::write(dir.join("zfile.txt"), b"").unwrap();
        std::fs::write(dir.join("afile"), b"").unwrap();

        let content = render_directory(&dir, "✗", " ");
        let _ = std::fs::remove_dir_all(&dir);

        match content {
            PreviewContent::Dir(entries) => {
                let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
                assert_eq!(names, vec!["adir", "bdir", "afile", "zfile.txt"]);
                assert!(entries[0].1 && entries[1].1 && !entries[2].1 && !entries[3].1);
            }
            other => panic!("expected Dir, got {other:?}"),
        }
    }

    #[test]
    fn empty_directory_is_empty_message() {
        let dir = test_dir();
        let content = render_directory(&dir, "✗", " ");
        let _ = std::fs::remove_dir_all(&dir);
        match content {
            PreviewContent::Message(lines) => {
                assert_eq!(lines, vec!["".to_string(), "--- ✗ Empty ---".to_string()]);
            }
            other => panic!("expected Empty Message, got {other:?}"),
        }
    }
}
