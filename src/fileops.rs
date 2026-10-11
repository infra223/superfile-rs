//! File operations: listing, copy/move, trash, zip, extract.
//!
//! Blocking work is executed in worker threads that report back through
//! `WorkerMsg` (the Rust analogue of superfile's `tea.Cmd` + process messages).

use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// true if this (or the symlink target) is a directory
    pub is_dir: bool,
    pub is_symlink: bool,
    /// file size; for directories: number of direct children (used by size sort)
    pub size: u64,
    pub mtime: SystemTime,
    /// lstat permission+type bits (Go: os.DirEntry.Info() is Lstat-based)
    pub mode: u32,
}

impl Entry {
    /// Lower-cased extension without the dot ("" when absent).
    pub fn ext(&self) -> std::borrow::Cow<'_, str> {
        match self.path.extension() {
            Some(e) => std::borrow::Cow::Owned(e.to_string_lossy().to_ascii_lowercase()),
            None => std::borrow::Cow::Borrowed(""),
        }
    }
}

pub fn list_dir(path: &Path, show_hidden: bool) -> Vec<Entry> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let lmeta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_symlink = lmeta.file_type().is_symlink();
        let meta = std::fs::metadata(&path).unwrap_or_else(|_| lmeta.clone());
        let is_dir = meta.is_dir();
        let size = if is_dir {
            std::fs::read_dir(&path).map(|r| r.count() as u64).unwrap_or(0)
        } else {
            meta.len()
        };
        let mtime = meta
            .modified()
            .ok()
            .or_else(|| lmeta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(Entry {
            name,
            path,
            is_dir,
            is_symlink,
            size,
            mtime,
            mode: lmeta.mode(),
        });
    }
    out
}

pub fn parent_of(p: &Path) -> PathBuf {
    p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------
// Worker protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Copy,
    Cut,
    Delete,
    Create,
    Compress,
    Extract,
}

#[derive(Debug)]
pub enum WorkerMsg {
    Progress {
        op: u64,
        file: String,
        done: usize,
        total: usize,
    },
    Fail {
        op: u64,
        file: String,
        err: String,
        remaining: Vec<String>,
    },
    Done {
        op: u64,
    },
}

/// Spawn a worker running `kind` over `items` (paths). `dest` is the
/// destination directory for copy/cut/create. Reports via `tx`.
///
/// Progress semantics (Go `makePasteProcessor` + `actualPasteOperation`, R2):
/// Copy and cross-device Cut report one `Progress` per copied file
/// (`file = base(path)`, absolute `done`/`total` where total is the recursive
/// file count); same-device Cut uses the fast whole-tree rename with no
/// per-file progress. Every op ends with a final `Progress { done: total }`
/// (Go `process.Done = process.Total` before `markProcessDone`).
///
/// `total` is computed once by the caller (`op_total`, or the compress
/// validation) and shared with the process-bar entry; `done_offset` is the
/// progress already reported when resuming after Skip (R5; 0 for fresh
/// ops); `use_trash` selects the trash backend for `Delete` (Go
/// `makeDeleteProcessor`'s `deleteFunc` choice).
///
/// `Compress` is a single process over all `items` (Go `zipSources`): one
/// archive written to `dest` (the explicit target path) with per-entry
/// progress. `Extract` unpacks `items[0]` into `dest` (the explicit output
/// directory).
pub fn spawn_worker(
    kind: OpKind,
    op_id: u64,
    items: Vec<PathBuf>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMsg>,
    total: usize,
    done_offset: usize,
    use_trash: bool,
) {
    std::thread::spawn(move || {
        fn progress_cb<'a>(
            tx: &'a Sender<WorkerMsg>,
            op_id: u64,
            done: &'a mut usize,
            total: usize,
        ) -> impl FnMut(&Path) + 'a {
            move |p: &Path| {
                *done += 1;
                let _ = tx.send(WorkerMsg::Progress {
                    op: op_id,
                    file: base_name(p),
                    done: *done,
                    total,
                });
            }
        }

        let mut done: usize = done_offset;

        // Compress is one process over all sources (Go `zipSources`): a
        // single archive at `dest` with per-entry progress; a walk failure
        // fails the process outright (no resume list, Go `zipSourcesCore`).
        if kind == OpKind::Compress {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerMsg::Done { op: op_id });
                return;
            }
            let mut last_file = String::new();
            let res = {
                let mut cb = |p: &Path| {
                    done += 1;
                    last_file = base_name(p);
                    let _ = tx.send(WorkerMsg::Progress {
                        op: op_id,
                        file: last_file.clone(),
                        done,
                        total,
                    });
                };
                compress_items(&items, &dest, Some(&mut cb)).map_err(|e| e.to_string())
            };
            match res {
                Ok(()) => {
                    // Go: p.Done = totalFiles before the final update.
                    let _ = tx.send(WorkerMsg::Progress {
                        op: op_id,
                        file: last_file,
                        done: total,
                        total,
                    });
                    let _ = tx.send(WorkerMsg::Done { op: op_id });
                }
                Err(err) => {
                    let _ = tx.send(WorkerMsg::Fail {
                        op: op_id,
                        file: base_name(&dest),
                        err,
                        remaining: Vec::new(),
                    });
                }
            }
            return;
        }

        for (i, item) in items.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerMsg::Done { op: op_id });
                return;
            }
            let res: Result<(), String> = match kind {
                OpKind::Copy => copy_item(item, &dest, Some(&mut progress_cb(&tx, op_id, &mut done, total)))
                    .map_err(|e| e.to_string()),
                OpKind::Cut => {
                    if is_same_partition(item, &dest) {
                        // Fast whole-tree rename; no per-file progress (R2).
                        move_item(item, &dest, None).map_err(|e| e.to_string())
                    } else {
                        move_item(item, &dest, Some(&mut progress_cb(&tx, op_id, &mut done, total)))
                            .map_err(|e| e.to_string())
                    }
                }
                OpKind::Delete => delete_item(item, use_trash).map_err(|e| e.to_string()).map(|_| done += 1),
                OpKind::Create => create_item(item).map_err(|e| e.to_string()).map(|_| done += 1),
                OpKind::Compress => unreachable!("Compress runs as a single process above"),
                OpKind::Extract => extract_archive(item, &dest).map_err(|e| e.to_string()).map(|_| done += 1),
            };
            match res {
                Ok(()) => {
                    // Go: process.CurrentFile = base(filePath); TrySendingUpdateProcessMsg
                    let _ = tx.send(WorkerMsg::Progress {
                        op: op_id,
                        file: base_name(item),
                        done,
                        total,
                    });
                }
                Err(err) => {
                    let remaining: Vec<String> = items[i + 1..]
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect();
                    let _ = tx.send(WorkerMsg::Fail {
                        op: op_id,
                        file: item.display().to_string(),
                        err,
                        remaining,
                    });
                    return;
                }
            }
        }
        let last_file = items.last().map(|p| base_name(p)).unwrap_or_default();
        let _ = tx.send(WorkerMsg::Progress {
            op: op_id,
            file: last_file,
            done: total,
            total,
        });
        let _ = tx.send(WorkerMsg::Done { op: op_id });
    });
}

/// Process-bar total for an op (R2): paste totals are recursive file counts,
/// other ops count items. Computed once by the app and passed to
/// `spawn_worker` (also used when resuming after Skip).
pub fn op_total(kind: OpKind, items: &[PathBuf]) -> usize {
    match kind {
        OpKind::Copy | OpKind::Cut => items.iter().map(|p| count_files(p)).sum(),
        _ => items.len(),
    }
}

/// Re-run a worker on the remaining items after the user pressed "Skip"
/// (Go `FileListErrorState.Skip` -> `runFileProcessor` with `fileList[1:]`).
/// `total` is the original process total and `done_offset` the progress
/// already reported (R5: `total - remaining.len()`); neither is recomputed
/// from `remaining`.
pub fn spawn_remaining(
    kind: OpKind,
    op_id: u64,
    remaining: Vec<String>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMsg>,
    total: usize,
    done_offset: usize,
    use_trash: bool,
) {
    let items: Vec<PathBuf> = remaining.iter().map(|s| PathBuf::from(s)).collect();
    spawn_worker(kind, op_id, items, dest, cancel, tx, total, done_offset, use_trash);
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// Copy `src` into directory `dest` (creating `dest/name`). Handles symlinks
/// with `cp -P` semantics and preserves permissions. `progress` is invoked
/// once per copied file/symlink (Go `actualPasteOperation`'s `p.Done++`).
pub fn copy_item(src: &Path, dest_dir: &Path, progress: Option<&mut dyn FnMut(&Path)>) -> std::io::Result<()> {
    let mut sink = |_p: &Path| {};
    let mut cb: &mut dyn FnMut(&Path) = match progress {
        Some(f) => f,
        None => &mut sink,
    };
    let name = src.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let target = unique_target(dest_dir.join(name));
    let lmeta = std::fs::symlink_metadata(src)?;
    if lmeta.file_type().is_symlink() {
        let link = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(link, &target)?;
        cb(src);
        return Ok(());
    }
    if lmeta.is_dir() {
        copy_dir_recursive(src, &target, &mut cb)?;
    } else {
        let perm = lmeta.permissions();
        std::fs::copy(src, &target)?;
        std::fs::set_permissions(&target, perm)?;
        cb(src);
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path, mut progress: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let src_perm = std::fs::metadata(src)?.permissions();
    std::fs::set_permissions(dst, src_perm)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let lmeta = std::fs::symlink_metadata(&e.path())?;
        let sp = e.path();
        let dp = dst.join(e.file_name());
        if lmeta.file_type().is_symlink() {
            let link = std::fs::read_link(&sp)?;
            std::os::unix::fs::symlink(link, &dp)?;
            progress(&sp);
        } else if lmeta.is_dir() {
            copy_dir_recursive(&sp, &dp, &mut progress)?;
        } else {
            std::fs::copy(&sp, &dp)?;
            std::fs::set_permissions(&dp, lmeta.permissions())?;
            progress(&sp);
        }
    }
    Ok(())
}

/// Move `src` into `dest_dir`. Uses rename when possible, else copy+remove
/// (Go `moveElement`). `progress` is invoked per copied file on the
/// copy+remove path only (the rename fast path has no per-file progress).
pub fn move_item(src: &Path, dest_dir: &Path, progress: Option<&mut dyn FnMut(&Path)>) -> std::io::Result<()> {
    let name = src.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let target = unique_target(dest_dir.join(name));
    if std::fs::rename(src, &target).is_ok() {
        return Ok(());
    }
    copy_item(src, dest_dir, progress)?;
    if src.is_dir() && !src.is_symlink() {
        std::fs::remove_dir_all(src)
    } else {
        std::fs::remove_file(src)
    }
}

/// Create a file or directory (Go `createItem`). A trailing `/` makes it a
/// directory (`create_dir_all`, like Go's `MkdirAll`); a file path goes
/// through `unique_target` (Go `renameIfDuplicate`, R1 `name(N).ext` format)
/// before creating parents + file.
///
/// Go `checkFileNameValidity` rejects `.` / `..` and names ending in `/.` or
/// `/..`; the worker receives the joined path, so the trailing `.`/`..`
/// component carries the rejection.
pub fn create_item(path: &Path) -> std::io::Result<()> {
    let s = path.display().to_string();
    if s == "." || s == ".." || s.ends_with("/.") || s.ends_with("/..") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file name cannot be '.' or '..'",
        ));
    }
    if s.ends_with('/') {
        std::fs::create_dir_all(path)
    } else {
        let deduped = unique_target(path.to_path_buf());
        if deduped == path && path.exists() {
            // unique_target exhausted its 10_000 counter and returned the
            // existing path: Go `renameIfDuplicate` errors here; opening it
            // with truncate(true) would silently wipe the existing file.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("could not find free name for {} after many attempts", path.display()),
            ));
        }
        let parent = deduped.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(p) = parent {
            std::fs::create_dir_all(p)?;
        }
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&deduped)
            .map(|_| ())
    }
}

/// Delete: to the OS trash when `use_trash` (Linux XDG), else permanent.
///
/// Go `makeDeleteProcessor`: when the user confirmed a *trash* delete, a
/// failing `trash_move` fails the process (error modal) — it must NOT fall
/// through to permanent removal, which would destroy data the user expected
/// to be recoverable.
pub fn delete_item(path: &Path, use_trash: bool) -> std::io::Result<()> {
    if use_trash {
        return trash_move(path);
    }
    if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

// ---------------------------------------------------------------------------
// XDG trash (Linux)
// ---------------------------------------------------------------------------

pub fn trash_root() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::data_dir());
    let data = data?;
    let trash = data.join("Trash");
    if trash.is_dir() {
        Some(trash)
    } else {
        None
    }
}

/// True when the trash is available (drives the confirmation wording).
pub fn has_trash() -> bool {
    trash_root().is_some()
}

/// Approximation of Go `trash.Available` + `selectTrashDir`: the trash root
/// exists, `path` is not inside a known trash dir (Go `isInsideKnownTrash`,
/// home-trash variant), and `path` shares the trash root's device (`st_dev`,
/// Go `sameDevice`). Cross-device paths would need a per-mount `.Trash-uid`
/// (Go's fallback), which this port does not create — reported unavailable
/// so the delete flow falls back to permanent deletion.
pub fn trash_available_for(path: &Path) -> bool {
    match trash_root() {
        Some(root) => trash_available_under(path, &root),
        None => false,
    }
}

/// `trash_available_for` with an explicit trash root (testable without
/// touching the process-global environment).
fn trash_available_under(path: &Path, root: &Path) -> bool {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root_abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if abs.starts_with(&root_abs) {
        return false;
    }
    match (std::fs::metadata(&abs), std::fs::metadata(&root_abs)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

pub fn trash_move(src: &Path) -> std::io::Result<()> {
    let root = trash_root().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Unsupported, "trash not available"))?;
    let files = root.join("files");
    let info = root.join("info");
    std::fs::create_dir_all(&files)?;
    std::fs::create_dir_all(&info)?;

    let abs = std::fs::canonicalize(src)?;
    let name = abs.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no name"))?;
    let target = unique_target(files.join(name));
    let info_target = unique_target(info.join(format!(".{:?}.trashinfo", name.to_string_lossy()).trim_matches('"')));

    let date = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let content = format!(
        "[Trash Info]\nPath={}\nDeletionDate={date}\n",
        crate::util::url_escape_path(&abs.display().to_string())
    );
    std::fs::write(&info_target, content)?;
    std::fs::rename(&abs, &target)
}

// ---------------------------------------------------------------------------
// Zip / extract
// ---------------------------------------------------------------------------

/// Go `IsExtensionExtractable` set (matched against the lowercased LAST
/// extension, like Go's `filepath.Ext`: `.xz`, `.tgz` and `.bz2` are NOT
/// extractable; `.tar.gz` is, via its `.gz` suffix).
pub const EXTRACTABLE_EXTS: &[&str] = &["zip", "bz", "gz", "iso", "rar", "7z", "tar"];

pub fn is_extractable(path: &Path) -> bool {
    path.extension()
        .map(|e| EXTRACTABLE_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Go `FileNameWithoutExtension`: strip every trailing dot-suffix from the
/// base name (a leading dot is kept: `.hidden.zip` -> `.hidden`,
/// `.gitignore` -> `.gitignore`), re-joined with the parent directory.
pub fn file_name_without_extension(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    loop {
        match name.rfind('.') {
            Some(pos) if pos > 0 => name.truncate(pos),
            _ => break,
        }
    }
    path.parent().unwrap_or(Path::new("")).join(name)
}

/// Go `getZipArchiveName` minus the `renameIfDuplicate` step: the app turns
/// an existing target into a Cancelled process instead (R6). `base` is the
/// first source's base name: strip the last extension (Go `filepath.Ext`)
/// and append `.zip`; a base starting with `.` that contains exactly one
/// `.` keeps its full name (`.gitignore` -> `.gitignore.zip`).
pub fn zip_archive_name(base: &str) -> Result<String, String> {
    if base.is_empty() {
        return Err("empty filename to compress".to_string());
    }
    let ext = match base.rfind('.') {
        Some(i) => &base[i..],
        None => "",
    };
    let mut zip_name = format!("{}.zip", &base[..base.len() - ext.len()]);
    if base.starts_with('.') && base.matches('.').count() == 1 {
        zip_name = format!("{base}.zip");
    }
    Ok(zip_name)
}

/// Go `validateCompressOperation`: every source must exist and be
/// accessible, plain files must be readable, and the recursive readable
/// file count is computed. Error strings match Go.
pub fn validate_compress_sources(sources: &[PathBuf]) -> Result<usize, String> {
    let mut total = 0usize;
    for src in sources {
        match std::fs::metadata(src) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("source path does not exist: {}", src.display()));
            }
            Err(e) => {
                return Err(format!("cannot access source path {}: {e}", src.display()));
            }
            Ok(m) => {
                if !m.is_dir() {
                    if let Err(e) = check_file_readable(src) {
                        return Err(format!("the file is not readable: {e}"));
                    }
                }
            }
        }
        total += match count_readable_files(src) {
            Ok(n) => n,
            Err(e) => {
                return Err(format!(
                    "error while counting files for {}: {e}",
                    src.display()
                ));
            }
        };
    }
    Ok(total)
}

/// Go `checkFileReadable`.
fn check_file_readable(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path).map(|_| ())
}

/// Go `countReadableFiles`: recursive count of readable non-directories
/// (symlinks count as one entry, like `filepath.Walk`); any unreadable
/// entry aborts the count with an error.
pub fn count_readable_files(path: &Path) -> Result<usize, String> {
    let lmeta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !lmeta.is_dir() {
        check_file_readable(path).map_err(|e| e.to_string())?;
        return Ok(1);
    }
    let mut count = 0usize;
    let rd = std::fs::read_dir(path).map_err(|e| e.to_string())?;
    for entry in rd {
        let p = entry.map_err(|e| e.to_string())?.path();
        let lmeta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
        if lmeta.is_dir() {
            count += count_readable_files(&p)?;
        } else {
            check_file_readable(&p).map_err(|e| e.to_string())?;
            count += 1;
        }
    }
    Ok(count)
}

/// Zip `items` into the explicit `target` path (Go `zipSources` +
/// `zipSourcesCore`): each source is stored relative to its own parent,
/// directories get a trailing-slash entry, symlinks are stored as their
/// link text. `progress` is invoked once per entry written (Go `p.Done++`
/// after `writeZipFile`). No dedup naming — the caller checked the target
/// (R6).
pub fn compress_items(
    items: &[PathBuf],
    target: &Path,
    progress: Option<&mut dyn FnMut(&Path)>,
) -> std::io::Result<()> {
    let mut sink = |_p: &Path| {};
    let cb: &mut dyn FnMut(&Path) = match progress {
        Some(f) => f,
        None => &mut sink,
    };
    let file = std::fs::File::create(target)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for item in items {
        // Go zipSourcesCore: relPath is relative to each source's own parent.
        let base = item.parent().unwrap_or(Path::new("")).to_path_buf();
        let mut stack = vec![item.to_path_buf()];
        while let Some(p) = stack.pop() {
            let lmeta = std::fs::symlink_metadata(&p)?;
            let relp = p.strip_prefix(&base).unwrap_or(&p);
            if lmeta.is_dir() {
                let name = format!("{}/", relp.display());
                zip.start_file(name, options)?;
                for e in std::fs::read_dir(&p)?.flatten() {
                    stack.push(e.path());
                }
            } else if lmeta.file_type().is_symlink() {
                let link = std::fs::read_link(&p)?;
                zip.start_file(relp.display().to_string(), options)?;
                zip.write_all(link.to_string_lossy().as_bytes())?;
            } else {
                let data = std::fs::read(&p)?;
                zip.start_file(relp.display().to_string(), options)?;
                zip.write_all(&data)?;
            }
            cb(&p);
        }
    }
    zip.finish()?;
    Ok(())
}

/// Extract an archive into the explicit output directory `dest` (Go
/// `extractCompressFile` with `xtractr.OutputDir`; the caller derives `dest`
/// from `file_name_without_extension` + `unique_target` and creates it).
pub fn extract_archive(path: &Path, dest: &Path) -> std::io::Result<()> {
    let lower = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "zip" => extract_zip(path, dest)?,
        "tar" => extract_tar(path, dest, None)?,
        "gz" => {
            if lower.ends_with(".tar.gz") {
                extract_tar(path, dest, Some(flate2::Compression::default()))?
            } else {
                // Plain gz: the single decoded file lands inside the output
                // dir (xtractr semantics).
                std::fs::create_dir_all(dest)?;
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "extracted".into());
                let out = dest.join(stem);
                let f = std::fs::File::create(&out)?;
                let mut src = std::fs::File::open(path)?;
                let mut g = flate2::write::GzDecoder::new(f);
                std::io::copy(&mut src, &mut g)?;
                g.finish()?;
            }
        }
        _ => {
            // fall back to external tools
            let tool = match ext.as_str() {
                "7z" => "7z",
                "rar" => "unrar",
                _ => "bsdtar",
            };
            run_external_extract(tool, path, dest)?;
        }
    }
    Ok(())
}

fn extract_zip(path: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad zip entry"))?;
        let out_path = dest.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

fn extract_tar(path: &Path, dest: &Path, gzip: Option<flate2::Compression>) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let file = std::fs::File::open(path)?;
    let rd: Box<dyn std::io::Read + '_> = match gzip {
        Some(_c) => Box::new(flate2::read::GzDecoder::new(file)),
        None => Box::new(file),
    };
    let mut tar = tar::Archive::new(rd);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .components()
            .filter(|c| !matches!(c, Component::ParentDir | Component::CurDir | Component::RootDir))
            .collect::<PathBuf>();
        if name.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest.join(&name);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&out_path)?;
    }
    Ok(())
}

fn run_external_extract(tool: &str, path: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut cmd = match tool {
        "7z" => {
            let mut c = std::process::Command::new("7z");
            c.arg("x").arg(format!("-o{}", dest.display())).arg(path);
            c
        }
        "unrar" => {
            let mut c = std::process::Command::new("unrar");
            c.arg("x").arg("-o+").arg(path).arg(dest.join("."));
            c
        }
        _ => {
            let mut c = std::process::Command::new(tool);
            c.arg("xf").arg(path).arg("-C").arg(dest);
            c
        }
    };
    let status = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| std::io::Error::new(e.kind(), format!("failed to run {tool}: {e}")))?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("{tool} exited with {status}"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// Go `renameIfDuplicate`: `dir/name.ext` -> `dir/name(1).ext`,
/// `dir/name(2).ext`, ... resuming the counter from an existing `(N)` suffix
/// (cap 10_000). On exhaustion returns the original path unchanged; callers
/// must treat "unchanged + exists" as exhaustion (Go returns an error there)
/// — `create_item` does, so it never truncates an existing file.
pub fn unique_target(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // Go: ext = filepath.Ext(base) — the suffix from the final dot, if any.
    let ext = match name.rfind('.') {
        Some(i) => name[i..].to_string(),
        None => String::new(),
    };
    let stem = &name[..name.len() - ext.len()];
    // Go suffixRegexp `^(.*)\((\d+)\)$` on the stem: resume from an existing (N).
    let mut base = stem.to_string();
    // Go parses the counter as int64; u64 + saturating keeps a huge existing
    // `(N)` suffix behaving like Go (counter past the 10_000 cap -> no free
    // name -> the original path is returned and the fs call fails).
    let mut counter: u64 = 1;
    if let Some(inner) = stem.strip_suffix(')') {
        if let Some(pos) = inner.rfind('(') {
            let digits = &inner[pos + 1..];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                base = inner[..pos].to_string();
                if let Ok(n) = digits.parse::<u64>() {
                    counter = n.saturating_add(1);
                }
            }
        }
    }
    for i in counter..10_000 {
        let cand = p.with_file_name(format!("{base}({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    p
}

/// Go `countFiles`: recursive count of non-directory entries (symlinks count
/// as one entry; directories themselves are not counted). Unreadable entries
/// are skipped rather than aborting the count.
pub fn count_files(path: &Path) -> usize {
    match std::fs::symlink_metadata(path) {
        Err(_) => 0,
        Ok(m) if !m.is_dir() => 1,
        Ok(_) => count_files_recursive(path),
    }
}

fn count_files_recursive(dir: &Path) -> usize {
    let mut count = 0;
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    for e in rd.flatten() {
        match std::fs::symlink_metadata(e.path()) {
            Ok(m) if m.is_dir() => count += count_files_recursive(&e.path()),
            Ok(_) => count += 1,
            Err(_) => {}
        }
    }
    count
}

/// Whether `src` and `dst` live on the same filesystem partition (Go
/// `isSamePartition`; Unix approximation via `st_dev` of the canonicalized
/// paths). Unknown metadata -> false (the conservative copy+remove path).
pub fn is_same_partition(src: &Path, dst: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match (std::fs::metadata(canon(src)), std::fs::metadata(canon(dst))) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// Check that `child` is not an ancestor of `parent` (paste validation).
pub fn is_ancestor(child: &Path, parent: &Path) -> bool {
    let Ok(c) = std::fs::canonicalize(child) else {
        return false;
    };
    let Ok(p) = std::fs::canonicalize(parent) else {
        return false;
    };
    c == p || p.starts_with(&c)
}

/// Go `validatePasteOperation`: reject pasting a cut item back into its own
/// parent directory, a directory into itself, or an item into itself or one
/// of its subdirectories. Error strings match Go verbatim.
pub fn validate_paste_operation(dest: &Path, items: &[PathBuf], cut: bool) -> Result<(), String> {
    for src in items {
        if cut && src.parent() == Some(dest) {
            return Err(format!(
                "cannot paste into parent directory of source, srcPath : {}, panelLocation : {}",
                src.display(),
                dest.display()
            ));
        }
        if cut && src == dest {
            return Err("cannot paste a directory into itself".to_string());
        }
        if is_ancestor(src, dest) {
            return Err(format!(
                "cannot {} and paste a directory into itself or its subdirectory",
                if cut { "cut" } else { "copy" }
            ));
        }
    }
    Ok(())
}

/// Wait with a timeout for a child process, killing it if needed.
pub fn wait_timeout(mut child: std::process::Child, timeout: Duration) -> std::io::Result<Option<std::process::ExitStatus>> {
    let start = std::time::Instant::now();
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(Some(st));
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Fresh unique temp directory per call.
    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("spf_fileops_{}_{}_{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, "x").unwrap();
        p
    }

    // ---- unique_target (Go renameIfDuplicate) --------------------------

    #[test]
    fn unique_target_no_collision_unchanged() {
        let dir = temp_dir("uniq_free");
        let p = dir.join("new.txt");
        assert_eq!(unique_target(p.clone()), p);
    }

    #[test]
    fn unique_target_go_name_n_ext_format() {
        let dir = temp_dir("uniq_fmt");
        touch(&dir, "a.txt");
        assert_eq!(unique_target(dir.join("a.txt")), dir.join("a(1).txt"));
        touch(&dir, "README");
        assert_eq!(unique_target(dir.join("README")), dir.join("README(1)"));
        // Non-numeric parens are part of the base name (Go regex mismatch).
        touch(&dir, "c(a)");
        assert_eq!(unique_target(dir.join("c(a)")), dir.join("c(a)(1)"));
    }

    #[test]
    fn unique_target_resumes_existing_counter() {
        let dir = temp_dir("uniq_resume");
        touch(&dir, "a.txt");
        touch(&dir, "a(1).txt");
        touch(&dir, "a(2).txt");
        assert_eq!(unique_target(dir.join("a.txt")), dir.join("a(3).txt"));
        // Input already carries the (N) suffix: resume from it.
        assert_eq!(unique_target(dir.join("a(2).txt")), dir.join("a(3).txt"));
        touch(&dir, "b(3).png");
        assert_eq!(unique_target(dir.join("b(3).png")), dir.join("b(4).png"));
    }

    // ---- count_files (Go countFiles) ------------------------------------

    #[test]
    fn count_files_counts_files_not_dirs() {
        let dir = temp_dir("count");
        touch(&dir, "a.txt");
        touch(&dir, "sub/b.txt");
        touch(&dir, "sub/deep/c.txt");
        std::fs::create_dir_all(dir.join("emptydir")).unwrap();
        assert_eq!(count_files(&dir), 3);
        assert_eq!(count_files(&dir.join("a.txt")), 1);
        assert_eq!(count_files(&dir.join("sub")), 2);
        assert_eq!(count_files(&dir.join("emptydir")), 0);
    }

    #[test]
    fn count_files_symlink_counts_once() {
        let dir = temp_dir("count_link");
        touch(&dir, "real/target.txt");
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        assert_eq!(count_files(&dir.join("link")), 1);
    }

    // ---- validate_paste_operation (Go validatePasteOperation) -----------

    #[test]
    fn validate_paste_cut_into_parent_rejected() {
        let dir = temp_dir("paste_parent");
        let src = touch(&dir, "a.txt");
        let err = paste_err(&dir, &[src.clone()], true);
        assert_eq!(
            err,
            Some(format!(
                "cannot paste into parent directory of source, srcPath : {}, panelLocation : {}",
                src.display(),
                dir.display()
            ))
        );
        // Copy into the same directory is allowed.
        assert_eq!(paste_err(&dir, &[src], false), None);
    }

    #[test]
    fn validate_paste_cut_directory_into_itself_rejected() {
        let dir = temp_dir("paste_self");
        let err = paste_err(&dir, &[dir.clone()], true);
        assert_eq!(err, Some("cannot paste a directory into itself".to_string()));
    }

    #[test]
    fn validate_paste_into_subdirectory_rejected() {
        let dir = temp_dir("paste_sub");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(
            paste_err(&dir.join("sub"), &[dir.clone()], false),
            Some("cannot copy and paste a directory into itself or its subdirectory".to_string())
        );
        assert_eq!(
            paste_err(&dir.join("sub"), &[dir.clone()], true),
            Some("cannot cut and paste a directory into itself or its subdirectory".to_string())
        );
        // Copy of a directory into itself also trips the ancestor check.
        assert_eq!(
            paste_err(&dir, &[dir.clone()], false),
            Some("cannot copy and paste a directory into itself or its subdirectory".to_string())
        );
    }

    #[test]
    fn validate_paste_happy_path() {
        let dir = temp_dir("paste_ok");
        let src_dir = temp_dir("paste_ok_src");
        let src = touch(&src_dir, "a.txt");
        let src_dir2 = touch(&src_dir, "tree/b.txt").parent().unwrap().to_path_buf();
        assert_eq!(paste_err(&dir, &[src, src_dir2], false), None);
        // Cut from an unrelated directory into this one is fine.
        let other = touch(&src_dir, "other.txt");
        assert_eq!(paste_err(&dir, &[other], true), None);
    }

    fn paste_err(dest: &Path, items: &[PathBuf], cut: bool) -> Option<String> {
        validate_paste_operation(dest, items, cut).err()
    }

    // ---- worker integration (temp dir) -----------------------------------

    fn collect_worker_msgs(rx: mpsc::Receiver<WorkerMsg>) -> Vec<WorkerMsg> {
        let mut msgs = Vec::new();
        for msg in rx.iter() {
            let done = matches!(msg, WorkerMsg::Done { .. });
            msgs.push(msg);
            if done {
                break;
            }
        }
        msgs
    }

    #[test]
    fn worker_copy_dir_tree_reports_per_file_progress() {
        let src_root = temp_dir("worker_copy_src");
        touch(&src_root, "tree/a.txt");
        touch(&src_root, "tree/sub/b.txt");
        touch(&src_root, "tree/sub/c.txt");
        let dest = temp_dir("worker_copy_dest");

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Copy,
            1,
            vec![src_root.join("tree")],
            dest.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            op_total(OpKind::Copy, &[src_root.join("tree")]),
            0,
            false,
        );
        let msgs = collect_worker_msgs(rx);

        let progresses: Vec<(String, usize, usize)> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { file, done, total, .. } => Some((file.clone(), *done, *total)),
                _ => None,
            })
            .collect();
        // R2: total is the recursive file count; per-file progress, absolute done.
        assert!(progresses.iter().all(|(_, _, total)| *total == 3));
        let dones: Vec<usize> = progresses.iter().map(|(_, d, _)| *d).collect();
        assert_eq!(&dones[..3], &[1, 2, 3][..]); // one Progress per copied file
        assert_eq!(*dones.last().unwrap(), 3); // final progress done = total
        // Progress carries the base name, not the full path.
        assert!(progresses.iter().all(|(f, _, _)| !f.contains('/') && !f.is_empty()));
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 1 })));

        // Source tree intact, destination tree complete.
        assert!(src_root.join("tree/sub/c.txt").exists());
        assert!(dest.join("tree/a.txt").exists());
        assert!(dest.join("tree/sub/b.txt").exists());
        assert!(dest.join("tree/sub/c.txt").exists());
        let _ = std::fs::remove_dir_all(&src_root);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn worker_same_device_cut_is_fast_rename() {
        let root = temp_dir("worker_cut");
        touch(&root, "src/a.txt");
        touch(&root, "src/b.txt");
        let dest = root.join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Cut,
            2,
            vec![root.join("src")],
            dest.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            2,
            0,
            false,
        );
        let msgs = collect_worker_msgs(rx);

        // R2: same-device cut renames the whole tree — no per-file progress,
        // only the final done = total update.
        let dones: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { done, .. } => Some(*done),
                _ => None,
            })
            .collect();
        assert_eq!(dones.last(), Some(&2));
        assert!(!dones[..dones.len() - 1].iter().any(|d| *d > 0 && *d < 2));
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 2 })));

        assert!(!root.join("src").exists());
        assert!(dest.join("src/a.txt").exists());
        assert!(dest.join("src/b.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_delete_permanent_fallback() {
        // use_trash = false: Go `makeDeleteProcessor` with `deleteFunc =
        // os.RemoveAll` — permanent deletion, no trash involvement.
        let root = temp_dir("worker_delete");
        touch(&root, "gone.txt");
        touch(&root, "tree/inner.txt");

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Delete,
            3,
            vec![root.join("gone.txt"), root.join("tree")],
            root.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            2,
            0,
            false,
        );
        let msgs = collect_worker_msgs(rx);

        // Delete totals count items (R2), one Progress per deleted item.
        let dones: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { done, total, .. } => {
                    assert_eq!(*total, 2);
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones, vec![1, 2, 2]);
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 3 })));

        assert!(!root.join("gone.txt").exists());
        assert!(!root.join("tree").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- trash_available_for (Go trash.Available approximation) ----------

    /// Serializes the tests that manipulate the process-global
    /// XDG_DATA_HOME (only these two touch it).
    static TRASH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn trash_available_for_cross_device_false() {
        let _guard = TRASH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // /proc is procfs: a different st_dev than the XDG trash root (and
        // Go's per-mount .Trash-uid fallback is not implemented here).
        assert!(!trash_available_for(Path::new("/proc")));
        if let Some(root) = trash_root() {
            // Inside a known trash dir -> false (Go isInsideKnownTrash).
            assert!(!trash_available_for(&root));
            assert!(!trash_available_for(&root.join("files")));
        }
    }

    #[test]
    fn trash_available_for_positive_and_negatives() {
        // Positive gate coverage with an explicit trash root (no env):
        // same-device outside the trash -> true; inside -> false;
        // cross-device -> false; missing root -> false.
        let base = temp_dir("trash_gate");
        let root = base.join("Trash");
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::create_dir_all(root.join("info")).unwrap();
        let target = touch(&base, "home/file.txt");

        assert!(trash_available_under(&target, &root));
        assert!(!trash_available_under(&root, &root));
        assert!(!trash_available_under(&root.join("files"), &root));
        assert!(!trash_available_under(&root.join("files/x"), &root));
        assert!(!trash_available_under(Path::new("/proc"), &root));

        // Same gate through the env-driven entry point (serialized).
        let _guard = TRASH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &base);
        let has = has_trash();
        let ok = trash_available_for(&target);
        let inside = trash_available_for(&root.join("files"));
        let cross = trash_available_for(Path::new("/proc"));
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(has);
        assert!(ok, "same-device path must be trash-available");
        assert!(!inside);
        assert!(!cross);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- create_item (Go createItem + checkFileNameValidity) -------------

    #[test]
    fn create_item_rejects_dot_names() {
        let dir = temp_dir("create_dot");
        for name in [".", "..", "sub/.", "sub/.."] {
            let err = create_item(&dir.join(name)).unwrap_err().to_string();
            assert!(
                err.contains("file name cannot be '.' or '..'"),
                "{name}: {err}"
            );
        }
        // Nothing was created.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!dir.join("sub").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_item_duplicate_uses_go_format() {
        let dir = temp_dir("create_dup");
        create_item(&dir.join("a.txt")).unwrap();
        create_item(&dir.join("a.txt")).unwrap(); // renameIfDuplicate -> a(1).txt
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("a(1).txt").exists());

        // Directory creation (trailing slash): MkdirAll, no dedup.
        create_item(&dir.join("newdir/")).unwrap();
        assert!(dir.join("newdir").is_dir());
        // Nested parents are created (Go MkdirAll(filepath.Dir(path))).
        create_item(&dir.join("x/y/file.txt")).unwrap();
        assert!(dir.join("x/y/file.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_target_huge_counter_like_go() {
        // T1-m3: Go parses the (N) suffix as int64. A counter beyond the
        // 10_000 cap means no free name -> Go errors; the helper returns the
        // original path and `create_item` turns that into the same error
        // (see create_item_exhausted_counter_errors_without_truncating).
        let dir = temp_dir("unique_huge");
        touch(&dir, "a(4294967296).txt"); // beyond u32, within u64
        let p = dir.join("a(4294967296).txt");
        assert_eq!(unique_target(p.clone()), p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_item_exhausted_counter_errors_without_truncating() {
        // Review fix 1: at/above the 10_000 cap unique_target returns the
        // original existing path; create_item must error (Go "could not find
        // free name") instead of opening it with truncate(true).
        let dir = temp_dir("create_exhaust");
        let at_cap = touch(&dir, "a(9999).txt"); // counter 10_000 -> range empty
        std::fs::write(&at_cap, "keep").unwrap();
        let huge = touch(&dir, "b(4294967296).txt"); // beyond u32
        std::fs::write(&huge, "keep2").unwrap();

        for p in [&at_cap, &huge] {
            let err = create_item(p).unwrap_err().to_string();
            assert!(err.contains("could not find free name"), "{p:?}: {err}");
        }
        // Existing contents untouched.
        assert_eq!(std::fs::read_to_string(&at_cap).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&huge).unwrap(), "keep2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- delete_item (Go makeDeleteProcessor) ------------------------------

    #[test]
    fn delete_item_trash_failure_does_not_permanently_delete() {
        // Review fix 1 (critical): with use_trash = true a failing trash_move
        // must fail the delete (Go error modal), NOT fall through to
        // remove_dir_all/remove_file and destroy the data.
        // Deterministic failure: Trash/files exists as a regular file, so
        // create_dir_all(files) fails regardless of user/permissions.
        let _guard = TRASH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("delete_trash_fail");
        let trash = dir.join("Trash");
        std::fs::create_dir_all(trash.join("info")).unwrap();
        std::fs::write(trash.join("files"), "blocker").unwrap();
        let victim = touch(&dir, "victim.txt");

        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &dir);
        let result = delete_item(&victim, true);
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(result.is_err(), "trash failure must propagate, not fall back");
        assert!(victim.exists(), "source must survive a failed trash delete");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- zip_archive_name (Go getZipArchiveName) -------------------------

    #[test]
    fn zip_archive_name_rules() {
        // Strip the last extension + ".zip".
        assert_eq!(zip_archive_name("notes.txt").unwrap(), "notes.zip");
        assert_eq!(zip_archive_name("a.tar.gz").unwrap(), "a.tar.zip");
        assert_eq!(zip_archive_name("plain").unwrap(), "plain.zip");
        // Dotfile with exactly one dot keeps its full name.
        assert_eq!(zip_archive_name(".gitignore").unwrap(), ".gitignore.zip");
        // More than one dot: normal rule applies.
        assert_eq!(zip_archive_name(".hidden.tar").unwrap(), ".hidden.zip");
        assert_eq!(
            zip_archive_name("").unwrap_err(),
            "empty filename to compress"
        );
    }

    // ---- validate_compress_sources (Go validateCompressOperation) --------

    #[test]
    fn validate_compress_sources_counts_recursively() {
        let dir = temp_dir("compress_validate");
        touch(&dir, "a.txt");
        touch(&dir, "tree/b.txt");
        touch(&dir, "tree/sub/c.txt");
        std::fs::create_dir_all(dir.join("emptydir")).unwrap();

        assert_eq!(validate_compress_sources(&[dir.clone()]).unwrap(), 3);
        assert_eq!(
            validate_compress_sources(&[dir.join("a.txt"), dir.join("tree")]).unwrap(),
            3
        );

        let missing = dir.join("nope.txt");
        let err = validate_compress_sources(&[missing.clone()]).unwrap_err();
        assert_eq!(err, format!("source path does not exist: {}", missing.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- compress_items (Go zipSources/zipSourcesCore) -------------------

    #[test]
    fn compress_items_writes_explicit_target_with_progress() {
        let src_root = temp_dir("compress_src");
        touch(&src_root, "tree/a.txt");
        touch(&src_root, "tree/sub/b.txt");
        let target = temp_dir("compress_dest").join("tree.zip");

        let mut seen: Vec<String> = Vec::new();
        compress_items(&[src_root.join("tree")], &target, Some(&mut |p: &Path| {
            seen.push(p.file_name().unwrap().to_string_lossy().to_string());
        }))
        .unwrap();

        assert!(target.is_file());
        // Progress per entry written (Go p.Done++): the source itself, the
        // nested dir, and both files.
        assert_eq!(seen.len(), 4);
        assert!(seen.contains(&"tree".to_string()));
        assert!(seen.contains(&"a.txt".to_string()));
        assert!(seen.contains(&"sub".to_string()));
        assert!(seen.contains(&"b.txt".to_string()));

        // Entries are relative to each source's own parent (Go relPath).
        let f = std::fs::File::open(&target).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["tree/", "tree/a.txt", "tree/sub/", "tree/sub/b.txt"]
        );
        let _ = std::fs::remove_dir_all(&src_root);
        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn worker_compress_is_single_process_with_progress() {
        let src_root = temp_dir("worker_compress_src");
        touch(&src_root, "a.txt");
        touch(&src_root, "tree/b.txt");
        touch(&src_root, "tree/c.txt");
        let dest_dir = temp_dir("worker_compress_dest");
        let target = dest_dir.join("a.zip");

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Compress,
            7,
            vec![src_root.join("a.txt"), src_root.join("tree")],
            target.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            3, // validation total (readable files); dir entries overshoot like Go
            0,
            false,
        );
        let msgs = collect_worker_msgs(rx);

        // Per-entry progress (Go p.Done++ counts dir entries too), then the
        // final update forces done = total (Go p.Done = totalFiles).
        let dones: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { done, total, .. } => {
                    assert_eq!(*total, 3);
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones, vec![1, 2, 3, 4, 3]);
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 7 })));

        assert!(target.is_file());
        let _ = std::fs::remove_dir_all(&src_root);
        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ---- extract gate + naming (Go IsExtensionExtractable /
    //      FileNameWithoutExtension) --------------------------------------

    #[test]
    fn is_extractable_gate_list() {
        // R7: {zip, bz, gz, iso, rar, 7z, tar} on the last extension.
        for name in ["a.zip", "a.bz", "a.gz", "a.iso", "a.rar", "a.7z", "a.tar"] {
            assert!(is_extractable(Path::new(name)), "{name}");
        }
        // Uppercase is lowercased like Go.
        assert!(is_extractable(Path::new("A.ZIP")));
        // .tar.gz is extractable via its .gz suffix.
        assert!(is_extractable(Path::new("a.tar.gz")));
        // NOT in the set: .xz, .tgz, .bz2 (and friends).
        for name in ["a.xz", "a.tgz", "a.bz2", "a.zst", "a.tar.bz2", "a.txt", "noext"] {
            assert!(!is_extractable(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn file_name_without_extension_strips_all_suffixes() {
        // R8: strip every trailing dot-suffix, keep a leading dot.
        let dir = Path::new("/p");
        assert_eq!(file_name_without_extension(&dir.join("a.tar.gz")), dir.join("a"));
        assert_eq!(file_name_without_extension(&dir.join("a.b.c")), dir.join("a"));
        assert_eq!(file_name_without_extension(&dir.join(".hidden.zip")), dir.join(".hidden"));
        assert_eq!(file_name_without_extension(&dir.join(".gitignore")), dir.join(".gitignore"));
        assert_eq!(file_name_without_extension(&dir.join("plain")), dir.join("plain"));
        // Extract output dir dedups with the Go (N) format (R1).
        let tmp = temp_dir("extract_naming");
        std::fs::create_dir_all(tmp.join("a")).unwrap();
        let stripped = file_name_without_extension(&tmp.join("a.zip"));
        assert_eq!(unique_target(stripped), tmp.join("a(1)"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn worker_extract_round_trip_into_explicit_dest() {
        let src = temp_dir("extract_rt_src");
        touch(&src, "tree/a.txt");
        touch(&src, "tree/sub/b.txt");
        let work = temp_dir("extract_rt_work");
        let archive = work.join("tree.zip");
        compress_items(&[src.join("tree")], &archive, None).unwrap();

        // Trigger-side naming: strip-all-suffixes + unique_target.
        let dest = unique_target(file_name_without_extension(&archive));
        assert_eq!(dest, work.join("tree"));
        std::fs::create_dir_all(&dest).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Extract,
            8,
            vec![archive.clone()],
            dest.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            1, // Go extractCompressFile total is 1
            0,
            false,
        );
        let msgs = collect_worker_msgs(rx);
        let dones: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { done, total, .. } => {
                    assert_eq!(*total, 1);
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones.last(), Some(&1));
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 8 })));

        assert!(dest.join("tree/a.txt").is_file());
        assert!(dest.join("tree/sub/b.txt").is_file());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&work);
    }

    // ---- skip/abort resume (Go FileListErrorState, R3-R5) ----------------

    #[test]
    fn worker_failure_reports_remaining_for_skip() {
        let dir = temp_dir("skip_fail");
        let missing = dir.join("missing.txt"); // fails first
        let a = touch(&dir, "a.txt");
        let b = touch(&dir, "b.txt");
        let dest = temp_dir("skip_fail_dest");

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Copy,
            9,
            vec![missing.clone(), a.clone(), b.clone()],
            dest.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            2,
            0,
            false,
        );
        let msgs = collect_worker_msgs(rx);
        match msgs.last() {
            Some(WorkerMsg::Fail { op, file, err, remaining }) => {
                assert_eq!(*op, 9);
                assert_eq!(*file, missing.display().to_string());
                assert!(!err.is_empty());
                // Go notProcessed = items[i:] minus the failed item (the
                // Skip key drops it before resuming).
                assert_eq!(
                    *remaining,
                    vec![a.display().to_string(), b.display().to_string()]
                );
            }
            other => panic!("expected Fail, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn skip_resume_flow_completes_process() {
        // R3: item 1 fails -> Skip -> the rest complete -> Done (the app
        // finishes the process Successful). R5: same op id, original total,
        // done_offset = total - remaining.len().
        let dir = temp_dir("skip_resume");
        let missing = dir.join("missing.txt");
        let a = touch(&dir, "a.txt");
        let b = touch(&dir, "b.txt");
        let dest = temp_dir("skip_resume_dest");
        let cancel = Arc::new(AtomicBool::new(false));

        let (tx, rx) = mpsc::channel();
        spawn_worker(
            OpKind::Copy,
            10,
            vec![missing.clone(), a.clone(), b.clone()],
            dest.clone(),
            cancel.clone(),
            tx,
            2,
            0,
            false,
        );
        let remaining = match collect_worker_msgs(rx).last() {
            Some(WorkerMsg::Fail { remaining, .. }) => remaining.clone(),
            other => panic!("expected Fail, got {other:?}"),
        };

        let (tx2, rx2) = mpsc::channel();
        spawn_remaining(
            OpKind::Copy,
            10,
            remaining,
            dest.clone(),
            cancel.clone(),
            tx2,
            2,
            2 - 2, // R5: total - remaining.len()
            false,
        );
        let msgs = collect_worker_msgs(rx2);
        let dones: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { done, total, .. } => {
                    assert_eq!(*total, 2); // original total, not recomputed
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones, vec![1, 1, 2, 2, 2]); // per-file cb + per-item update + final
        // Done on the SAME op id -> the app marks the process Successful (R3).
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 10 })));
        assert!(dest.join("a.txt").is_file());
        assert!(dest.join("b.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn spawn_remaining_uses_explicit_total_and_done_offset() {
        // R5: totals/offsets come from the caller, never recomputed from
        // `remaining` (a resumed copy must continue the absolute progress).
        let dir = temp_dir("resume_offset");
        let a = touch(&dir, "a.txt");
        let b = touch(&dir, "b.txt");
        let dest = temp_dir("resume_offset_dest");

        let (tx, rx) = mpsc::channel();
        spawn_remaining(
            OpKind::Copy,
            11,
            vec![a.display().to_string(), b.display().to_string()],
            dest.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            5,
            3,
            false,
        );
        let msgs = collect_worker_msgs(rx);
        let dones: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMsg::Progress { done, total, .. } => {
                    assert_eq!(*total, 5);
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones, vec![4, 4, 5, 5, 5]); // continues from done_offset = 3
        assert!(matches!(msgs.last(), Some(WorkerMsg::Done { op: 11 })));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
