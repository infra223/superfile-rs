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
    pub fn ext(&self) -> std::borrow::Cow<str> {
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

impl OpKind {
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
pub fn spawn_worker(
    kind: OpKind,
    op_id: u64,
    items: Vec<PathBuf>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMsg>,
) {
    let total = items.len();
    std::thread::spawn(move || {
        for (i, item) in items.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerMsg::Done { op: op_id });
                return;
            }
            let res: Result<(), String> = match kind {
                OpKind::Copy => copy_item(item, &dest).map_err(|e| e.to_string()),
                OpKind::Cut => move_item(item, &dest).map_err(|e| e.to_string()),
                OpKind::Delete => delete_item(item, true).map_err(|e| e.to_string()),
                OpKind::Create => create_item(item).map_err(|e| e.to_string()),
                OpKind::Compress => compress_items(&[item.clone()], &dest).map(|_| ()).map_err(|e| e.to_string()),
                OpKind::Extract => extract_archive(item).map(|_| ()).map_err(|e| e.to_string()),
            };
            match res {
                Ok(()) => {
                    let _ = tx.send(WorkerMsg::Progress {
                        op: op_id,
                        file: item.display().to_string(),
                        done: i + 1,
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
        let _ = tx.send(WorkerMsg::Done { op: op_id });
    });
}

/// Re-run a worker on the remaining items after the user pressed "Skip".
pub fn spawn_remaining(
    kind: OpKind,
    op_id: u64,
    remaining: Vec<String>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMsg>,
) {
    let items: Vec<PathBuf> = remaining.iter().map(|s| PathBuf::from(s)).collect();
    spawn_worker(kind, op_id, items, dest, cancel, tx);
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// Copy `src` into directory `dest` (creating `dest/name`). Handles symlinks
/// with `cp -P` semantics and preserves permissions.
pub fn copy_item(src: &Path, dest_dir: &Path) -> std::io::Result<()> {
    let name = src.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let target = unique_target(dest_dir.join(name));
    let lmeta = std::fs::symlink_metadata(src)?;
    if lmeta.file_type().is_symlink() {
        let link = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(link, &target)?;
        return Ok(());
    }
    if lmeta.is_dir() {
        copy_dir_recursive(src, &target)?;
    } else {
        let perm = lmeta.permissions();
        std::fs::copy(src, &target)?;
        std::fs::set_permissions(&target, perm)?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
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
        } else if lmeta.is_dir() {
            copy_dir_recursive(&sp, &dp)?;
        } else {
            std::fs::copy(&sp, &dp)?;
            std::fs::set_permissions(&dp, lmeta.permissions())?;
        }
    }
    Ok(())
}

/// Move `src` into `dest_dir`. Uses rename when possible, else copy+remove.
pub fn move_item(src: &Path, dest_dir: &Path) -> std::io::Result<()> {
    let name = src.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let target = unique_target(dest_dir.join(name));
    if std::fs::rename(src, &target).is_ok() {
        return Ok(());
    }
    copy_item(src, dest_dir)?;
    std::fs::remove_dir_all(src)
}

/// Create a file or directory. A trailing `/` makes it a directory.
pub fn create_item(path: &Path) -> std::io::Result<()> {
    let s = path.display().to_string();
    if s.ends_with('/') {
        std::fs::create_dir_all(path)
    } else {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(p) = parent {
            std::fs::create_dir_all(p)?;
        }
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)
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

pub const EXTRACTABLE_EXTS: &[&str] = &["zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "7z", "rar", "zst", "lz"];

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

/// `dir/name` -> `dir/name.1`, `dir/name.2`, ... picking a free target.
pub fn unique_target(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    let ext = p.extension().map(|e| e.to_os_string());
    for i in 1u32..10_000 {
        let mut cand = PathBuf::from(&stem);
        cand.push(format!(".{i}"));
        if let Some(e) = &ext {
            cand.set_extension(e);
        }
        cand = p.with_file_name(cand);
        if !cand.exists() {
            return cand;
        }
    }
    p
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
