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
// Delete/Create/Compress/Extract are constructed by the delete/create/
// compress/extract triggers (later Task B tasks).
#[allow(dead_code)]
pub enum OpKind {
    Copy,
    Cut,
    Delete,
    Create,
    Compress,
    Extract,
}

impl OpKind {
    #[allow(dead_code)] // reserved for process-bar labels
    pub fn label(&self) -> &'static str {
        match self {
            OpKind::Copy => "copy",
            OpKind::Cut => "cut",
            OpKind::Delete => "delete",
            OpKind::Create => "create",
            OpKind::Compress => "compress",
            OpKind::Extract => "extract",
        }
    }
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
/// `total` is computed once by the caller (`op_total`) and shared with the
/// process-bar entry; `use_trash` selects the trash backend for `Delete`
/// (Go `makeDeleteProcessor`'s `deleteFunc` choice).
pub fn spawn_worker(
    kind: OpKind,
    op_id: u64,
    items: Vec<PathBuf>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMsg>,
    total: usize,
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

        let mut done: usize = 0;
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
                OpKind::Compress => compress_items(&[item.clone()], &dest).map(|_| done += 1).map_err(|e| e.to_string()),
                OpKind::Extract => extract_archive(item).map(|_| done += 1).map_err(|e| e.to_string()),
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

/// Re-run a worker on the remaining items after the user pressed "Skip".
#[allow(dead_code)] // wired by the skip/abort flow (Task 3)
pub fn spawn_remaining(
    kind: OpKind,
    op_id: u64,
    remaining: Vec<String>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMsg>,
    use_trash: bool,
) {
    let items: Vec<PathBuf> = remaining.iter().map(|s| PathBuf::from(s)).collect();
    let total = op_total(kind, &items);
    spawn_worker(kind, op_id, items, dest, cancel, tx, total, use_trash);
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
        let path = unique_target(path.to_path_buf());
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(p) = parent {
            std::fs::create_dir_all(p)?;
        }
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&path)
            .map(|_| ())
    }
}

/// Delete: to the OS trash when available (Linux XDG), else permanent.
pub fn delete_item(path: &Path, use_trash: bool) -> std::io::Result<()> {
    if use_trash {
        if let Ok(res) = trash_move(path) {
            return Ok(res);
        }
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
    let root = match trash_root() {
        Some(r) => r,
        None => return false,
    };
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root_abs = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
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

// Consumed by the compress/extract triggers (Task 3).
#[allow(dead_code)]
pub const EXTRACTABLE_EXTS: &[&str] = &["zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "7z", "rar", "zst", "lz"];

#[allow(dead_code)] // used by the extract trigger (Task 3)
pub fn is_extractable(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") {
        return true;
    }
    path.extension()
        .map(|e| EXTRACTABLE_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn compress_archive_name(items: &[PathBuf], dest_dir: &Path) -> PathBuf {
    let base = if items.len() == 1 {
        items[0]
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "archive".into())
    } else {
        "items".to_string()
    };
    unique_target(dest_dir.join(format!("{base}.zip")))
}

pub fn compress_items(items: &[PathBuf], dest_dir: &Path) -> std::io::Result<PathBuf> {
    let out = compress_archive_name(items, dest_dir);
    let file = std::fs::File::create(&out)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let base = common_parent(items);
    for item in items {
        let _rel = item.strip_prefix(&base).unwrap_or(item);
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
        }
    }
    zip.finish()?;
    Ok(out)
}

fn common_parent(items: &[PathBuf]) -> PathBuf {
    if items.is_empty() {
        return PathBuf::from(".");
    }
    let mut parent = items[0].parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for item in items {
        let p = item.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        while !p.starts_with(&parent) && !parent.starts_with(&p) {
            parent = parent_of(&parent);
            if parent.as_os_str().is_empty() {
                parent = PathBuf::from("/");
                break;
            }
        }
    }
    if parent.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        parent
    }
}

/// Extract an archive into a sibling directory named after it.
pub fn extract_archive(path: &Path) -> std::io::Result<PathBuf> {
    let lower = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let stem = if lower.ends_with(".tar.gz") || lower.ends_with(".tar.bz2") || lower.ends_with(".tar.xz") {
        path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".tar"))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "extracted".into())
    } else {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "extracted".into())
    };
    let dest_dir = path.parent().unwrap_or(Path::new("."));
    let dest = unique_target(dest_dir.join(&stem));

    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "zip" => extract_zip(path, &dest)?,
        "tar" => extract_tar(path, &dest, None)?,
        "gz" | "tgz" => {
            if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
                extract_tar(path, &dest, Some(flate2::Compression::default()))?
            } else {
                let out = unique_target(dest_dir.join(&stem));
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
                "bz2" | "tbz2" | "xz" | "lz" | "zst" => "tar",
                _ => "bsdtar",
            };
            run_external_extract(tool, path, &dest)?;
        }
    }
    Ok(dest)
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
/// (cap 10_000). On exhaustion returns the original path; the subsequent fs
/// call then fails, mirroring Go's error path.
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

    #[test]
    fn trash_available_for_cross_device_false() {
        // /proc is procfs: a different st_dev than the XDG trash root (and
        // Go's per-mount .Trash-uid fallback is not implemented here).
        assert!(!trash_available_for(Path::new("/proc")));
        if let Some(root) = trash_root() {
            // Inside a known trash dir -> false (Go isInsideKnownTrash).
            assert!(!trash_available_for(&root));
            assert!(!trash_available_for(&root.join("files")));
        }
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
        // 10_000 cap means no free name -> Go errors; here the original path
        // is returned so the subsequent fs call fails (same op outcome).
        let dir = temp_dir("unique_huge");
        touch(&dir, "a(4294967296).txt"); // beyond u32, within u64
        let p = dir.join("a(4294967296).txt");
        assert_eq!(unique_target(p.clone()), p);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
