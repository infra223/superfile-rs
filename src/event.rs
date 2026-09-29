//! Asynchronous messages flowing from worker threads back to the UI,
//! the Rust analogue of superfile's `tea.Msg` / `tea.Cmd` pairs.

use std::path::PathBuf;

use crate::fileops::Entry;

/// Content produced by a computed file preview.
#[derive(Debug)]
pub enum PreviewContent {
    /// A short informational message (no file info, unsupported mode/format,
    /// image preview disabled, empty file, or an error).
    Message(Vec<String>),
    /// Text file content (already printable, one line per entry).
    Text(Vec<String>),
    /// Directory listing: `(name, is_dir, is_link)` per entry, sorted
    /// (directories first, then name ascending). Icons are resolved at render
    /// time by the preview component.
    Dir(Vec<(String, bool, bool)>),
    /// Half-block image: one row per `▄` row; each cell holds
    /// `(lower_pixel, upper_pixel)` where each pixel is `None` when
    /// transparent (component substitutes the panel background color).
    Image {
        rows: Vec<Vec<(Option<[u8; 3]>, Option<[u8; 3]>)>>,
    },
}

#[derive(Debug)]
pub enum AsyncMsg {
    /// A directory listing finished.
    DirListed {
        req_id: u64,
        path: PathBuf,
        entries: Vec<Entry>,
    },
    /// A background file operation made progress.
    OpProgress {
        op: u64,
        file: String,
        done: usize,
        total: usize,
    },
    /// A background file operation hit an error (user can Skip or Abort).
    OpFail {
        op: u64,
        file: String,
        err: String,
        remaining: Vec<String>,
    },
    /// A background file operation finished (successfully or cancelled).
    OpDone {
        op: u64,
        cancelled: bool,
    },
    /// Metadata for the previewed file (basic rows + exiftool fields).
    Metadata {
        req_id: u64,
        path: PathBuf,
        rows: Vec<(String, String)>,
        /// Non-empty when metadata could not be fully loaded.
        info_msg: String,
    },
    /// MD5 checksum computed.
    Md5 {
        req_id: u64,
        path: PathBuf,
        hash: String,
    },
    /// Zoxide fuzzy query results as (score, path) pairs.
    ZoxideQuery {
        req_id: u64,
        results: Vec<(f64, String)>,
    },
    /// Shell command result for the `:` prompt.
    ShellDone {
        req_id: u64,
        /// -1 when the command timed out.
        ret_code: i32,
        /// Combined stdout + stderr.
        output: String,
        /// Exec-level error (e.g. spawn failure), if any.
        err: Option<String>,
    },
    /// $() substitution result for the `>` prompt.
    SubstDone {
        req_id: u64,
        /// Ok(resolved line) or Err(error message from tokenization).
        resolved: Result<String, String>,
    },
    /// A file preview finished computing.
    Preview {
        req_id: u64,
        path: PathBuf,
        content: PreviewContent,
    },
    /// Latest upstream version (update check).
    UpdateCheck {
        latest: Option<String>,
    },
}
