//! Go `internal/ui/metadata` port (Model + GetMetadata + rendering; Linux only).
//
// NOTE — approved deviations from the Go original:
// 1. exiftool: per-call `exiftool -j -q` with a 10s timeout (Go: persistent `-stay_open`
//    process via barasher/go-exiftool, no per-call timeout).
// 2. Owner/Group: numeric id fallback when /etc/passwd|/etc/group lookup misses
//    (Go returns "").
// 3. Date Modified: chrono `%.f %:z` → `2024-01-15 12:30:00.500 +02:00`
//    (Go: `2024-01-15 10:30:00.5 +0000 UTC`; ms precision, colon offset, no zone name).
// 4. Unmapped ELF/Mach-O machine/cpu → `Unknown (0x..)` (Go: `machine.String()` / `cpu.String()`).
// 5. No cache (Go 300-entry/5-min) — the app dedupes via the expected location/focused.
// 6. No ELF class/version, PE section, or Mach-O sub-image validation (only magic/len
//    guards) — corrupt-file edge cases may differ.
// 7. Narrow-width guard: when key_len/value_len < 3 the value is char-cut instead of
//    `truncate_middle` (Go would panic there).

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{Config, Palette};
use crate::render::{BorderSet, RBuf, St};
use crate::util::{format_file_size, truncate_middle};

use md5::digest::Digest;

// ---------------------------------------------------------------------------
// Data + message types
// ---------------------------------------------------------------------------

pub struct MetadataData {
    pub filepath: String,
    pub rows: Vec<(String, String)>,
    pub info_msg: String,
}

pub struct MetadataMsg {
    pub req_id: u64,
    pub data: MetadataData,
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

pub struct Metadata {
    data: MetadataData,
    expected_location: String,
    expected_focused: bool,
    render_index: usize,
    #[allow(dead_code)]
    width: usize,
    height: usize,
}

impl Metadata {
    pub fn new() -> Self {
        Self {
            data: MetadataData {
                filepath: String::new(),
                rows: Vec::new(),
                info_msg: String::new(),
            },
            expected_location: String::new(),
            expected_focused: false,
            render_index: 0,
            width: 0,
            height: 0,
        }
    }

    pub fn set_dimensions(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
    }

    pub fn set_blank(&mut self) {
        self.data.filepath = String::new();
        self.data.rows.clear();
        self.data.info_msg = "No metadata present".to_string();
    }

    pub fn set_metadata(&mut self, mut data: MetadataData, focused: bool) {
        sort_rows(&mut data.rows);
        self.expected_location = data.filepath.clone();
        self.expected_focused = focused;
        self.data = data;
        self.reset_render_if_invalid();
    }

    pub fn set_info_msg(&mut self, msg: &str) {
        self.data.info_msg = msg.to_string();
    }

    pub fn set_location_and_focused(&mut self, location: &str, focused: bool) {
        self.expected_location = location.to_string();
        self.expected_focused = focused;
    }

    pub fn filepath(&self) -> Option<&str> {
        if self.data.filepath.is_empty() {
            None
        } else {
            Some(&self.data.filepath)
        }
    }

    pub fn get_location(&self) -> &str {
        &self.expected_location
    }

    pub fn get_expected_focused(&self) -> bool {
        self.expected_focused
    }

    pub fn is_blank(&self) -> bool {
        self.data.rows.is_empty() && self.data.info_msg.is_empty()
    }

    pub fn len(&self) -> usize {
        self.data.rows.len()
    }

    pub fn list_up(&mut self) {
        self.move_render_index_by(-1);
    }

    pub fn list_down(&mut self) {
        self.move_render_index_by(1);
    }

    pub fn page_up(&mut self, config: &Config) {
        self.move_render_index_by(-self.page_scroll_size(config));
    }

    pub fn page_down(&mut self, config: &Config) {
        self.move_render_index_by(self.page_scroll_size(config));
    }

    pub fn reset_render(&mut self) {
        self.render_index = 0;
    }

    pub fn reset_render_if_invalid(&mut self) {
        if self.render_index >= self.data.rows.len() {
            self.render_index = 0;
        }
    }

    fn move_render_index_by(&mut self, delta: i64) {
        let l = self.data.rows.len();
        if l == 0 {
            return;
        }
        self.render_index = ((self.render_index as i64 + delta).rem_euclid(l as i64)) as usize;
    }

    fn page_scroll_size(&self, config: &Config) -> i64 {
        let scroll = if config.page_scroll_size > 0 {
            config.page_scroll_size as i64
        } else {
            self.height.saturating_sub(2) as i64
        };
        scroll.max(1)
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        focused: bool,
        pal: &Palette,
    ) {
        buf.fill(x, y, w, h, St::fg_bg(pal.footer_bg, pal.footer_bg));
        if w < 2 || h < 2 {
            return;
        }
        let border_fg = if focused {
            pal.footer_border_active
        } else {
            pal.footer_border
        };
        buf.border_title(
            x, y, w, h,
            BorderSet::plain(),
            border_fg,
            pal.footer_bg,
            "Metadata",
            border_fg,
        );
        let cx = x + 1;
        let cw = w.saturating_sub(2);

        if self.data.rows.is_empty() {
            buf.put_line(cx, y + 1, cw, "", St::fg_bg(pal.footer_fg, pal.footer_bg));
            let msg = format!(" {}", self.data.info_msg);
            buf.put_line(cx, y + 2, cw, &msg, St::fg_bg(pal.footer_fg, pal.footer_bg));
            return;
        }

        let len = self.data.rows.len();
        let info = format!("{}/{}", self.render_index + 1, len);
        buf.border_info(
            x, y, w, h,
            BorderSet::plain(),
            border_fg,
            pal.footer_bg,
            &[info.as_str()],
        );

        let max_key_len = self
            .data
            .rows
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(0);
        let view_w = w.saturating_sub(4) as i64;
        let (key_len, value_len) = compute_metadata_widths(view_w, max_key_len as i64);

        let view_h = h.saturating_sub(2);
        let end = (self.render_index + view_h).min(len);
        for i in self.render_index..end {
            let (key, value) = &self.data.rows[i];
            let key_cut = if key_len >= 3 {
                truncate_middle(key, key_len as usize, "...")
            } else {
                char_cut(key, key_len as usize)
            };
            let pad = (key_len as usize).saturating_sub(key_cut.len());
            let mut line = String::new();
            line.push_str(&key_cut);
            for _ in 0..pad {
                line.push(' ');
            }
            line.push(' ');
            let value_cut = if value_len >= 3 {
                truncate_middle(value, value_len as usize, "...")
            } else {
                char_cut(value, value_len as usize)
            };
            line.push_str(&value_cut);
            buf.put_line(
                cx,
                y + 1 + (i - self.render_index),
                cw,
                &line,
                St::fg_bg(pal.footer_fg, pal.footer_bg),
            );
        }
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Sorting / layout helpers
// ---------------------------------------------------------------------------

fn priority(key: &str) -> Option<u8> {
    match key {
        "Name" => Some(0),
        "Size" => Some(1),
        "Date Modified" => Some(2),
        "Date Accessed" => Some(3),
        "Permissions" => Some(4),
        "Owner" => Some(5),
        "Group" => Some(6),
        "Path" => Some(7),
        "Architecture" => Some(8),
        _ => None,
    }
}

fn sort_rows(rows: &mut Vec<(String, String)>) {
    rows.sort_by(|a, b| {
        use std::cmp::Ordering;
        match (priority(&a.0), priority(&b.0)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.0.cmp(&b.0),
        }
    });
}

fn char_cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn compute_metadata_widths(view_width: i64, max_key_len: i64) -> (i64, i64) {
    let mut key_len = max_key_len;
    let mut value_len = view_width - key_len;
    if value_len < view_width / 2 {
        value_len = view_width / 2;
        key_len = view_width - value_len;
    }
    (key_len.max(0), value_len.max(0))
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

pub fn mode_string(mode: u32) -> String {
    let mut buf = ['\0'; 10];
    buf[0] = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let perm = mode & 0o777;
    for i in 0..3 {
        let p = (perm >> (3 * (2 - i) as u32)) & 0o7;
        for j in 0..3 {
            let idx = i * 3 + 1 + j;
            buf[idx] = if (p & (0x4 >> j as u32)) == 0 {
                '-'
            } else {
                match j {
                    0 => 'r',
                    1 => 'w',
                    _ => 'x',
                }
            };
        }
    }
    if mode & 0o4000 != 0 {
        buf[3] = if buf[3] == 'x' { 's' } else { 'S' };
    }
    if mode & 0o2000 != 0 {
        buf[6] = if buf[6] == 'x' { 's' } else { 'S' };
    }
    if mode & 0o1000 != 0 {
        buf[9] = if buf[9] == 'x' { 't' } else { 'T' };
    }
    buf.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Worker: collection
// ---------------------------------------------------------------------------

pub fn spawn_metadata(
    req_id: u64,
    path: String,
    focused: bool,
    config: Config,
    tx: mpsc::Sender<MetadataMsg>,
) {
    std::thread::spawn(move || {
        let data = collect_metadata(&path, focused, &config);
        let _ = tx.send(MetadataMsg { req_id, data });
    });
}

fn collect_metadata(path: &str, focused: bool, config: &Config) -> MetadataData {
    let p = Path::new(path);
    let mut data = MetadataData {
        filepath: path.to_string(),
        rows: Vec::new(),
        info_msg: String::new(),
    };

    match std::fs::symlink_metadata(path) {
        Err(_) => {
            data.info_msg = "Cannot load file stats".to_string();
        }
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                match std::fs::canonicalize(path) {
                    Err(_) => {
                        data.info_msg = "Link file is broken!".to_string();
                    }
                    Ok(target) => {
                        data.rows.push((
                            "Path".to_string(),
                            target.to_string_lossy().into_owned(),
                        ));
                    }
                }
            } else {
                let is_dir = meta.file_type().is_dir();
                let is_reg = meta.file_type().is_file();

                // Name
                let name = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                data.rows.push(("Name".to_string(), name.to_string()));

                // Size
                let size = if is_dir {
                    if focused {
                        format_file_size(dir_size(p), config.file_size_use_si)
                    } else {
                        "(focus)".to_string()
                    }
                } else {
                    format_file_size(meta.len(), config.file_size_use_si)
                };
                data.rows.push(("Size".to_string(), size));

                // Date Modified
                data.rows.push((
                    "Date Modified".to_string(),
                    format_modified(meta.modified()),
                ));

                // Permissions
                let mode = meta.permissions().mode();
                data.rows.push(("Permissions".to_string(), mode_string(mode)));

                // Owner / Group
                let uid = meta.uid();
                let gid = meta.gid();
                data.rows.push(("Owner".to_string(), lookup_user(uid)));
                data.rows.push(("Group".to_string(), lookup_group(gid)));

                // Architecture (regular files only)
                if is_reg {
                    if let Some(arch) = binary_architecture(p) {
                        data.rows.push(("Architecture".to_string(), arch));
                    }
                }

                // Exiftool (only if enabled)
                if config.metadata {
                    apply_exiftool(path, &mut data);
                }

                // MD5 (regular files && enabled)
                if is_reg && config.enable_md5_checksum {
                    if let Some(sum) = md5_checksum(p) {
                        data.rows.push(("MD5Checksum".to_string(), sum));
                    }
                }
            }
        }
    }

    data
}

fn format_modified(modified: std::io::Result<SystemTime>) -> String {
    let Ok(t) = modified else {
        return String::new();
    };
    let Ok(d) = t.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
        .map(|t| t.with_timezone(&chrono::Local))
        .unwrap_or_else(chrono::Local::now)
        .format("%Y-%m-%d %H:%M:%S%.f %:z")
        .to_string()
}

fn lookup_user(uid: u32) -> String {
    lookup_in_file("/etc/passwd", uid).unwrap_or_else(|| uid.to_string())
}

fn lookup_group(gid: u32) -> String {
    lookup_in_file("/etc/group", gid).unwrap_or_else(|| gid.to_string())
}

fn lookup_in_file(path: &str, id: u32) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() > 2 {
            if let Ok(fid) = fields[2].parse::<u32>() {
                if fid == id {
                    return Some(fields[0].to_string());
                }
            }
        }
    }
    None
}

fn dir_size(path: &Path) -> u64 {
    let mut size: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if md.is_dir() {
                stack.push(p);
            } else {
                size = size.saturating_add(md.len());
            }
        }
    }
    size
}

fn md5_checksum(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut h = md5::Md5::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let out = h.finalize();
    Some(out.iter().map(|b| format!("{:02x}", b)).collect())
}

fn apply_exiftool(path: &str, data: &mut MetadataData) {
    let path_owned = path.to_string();
    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::spawn(move || {
        let result = std::process::Command::new("exiftool")
            .args(["-j", "-q"])
            .arg(&path_owned)
            .output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(10)) {
        Err(_) => {
            // timeout
            data.info_msg = "Errors while fetching metadata via exiftool".to_string();
        }
        Ok(Err(_)) => {
            // spawn failure (e.g. exiftool not installed) → silent skip (Go et == nil)
        }
        Ok(Ok(out)) => {
            if !out.status.success() {
                data.info_msg = "Errors while fetching metadata via exiftool".to_string();
            } else {
                match serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) {
                    Ok(arr) => {
                        if let Some(first) = arr.first() {
                            if let serde_json::Value::Object(map) = first {
                                for (k, v) in map {
                                    data.rows.push((k.clone(), value_str(v)));
                                }
                            }
                        }
                    }
                    Err(_) => {
                        data.info_msg = "Errors while fetching metadata via exiftool".to_string();
                    }
                }
            }
        }
    }
}

fn value_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(value_str).collect();
            format!("[{}]", inner.join(" "))
        }
        serde_json::Value::Null => "<nil>".to_string(),
        serde_json::Value::Object(_) => v.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Binary architecture detection
// ---------------------------------------------------------------------------

fn binary_architecture(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_err() {
        return None;
    }

    // ELF
    if magic == [0x7f, b'E', b'L', b'F'] {
        return elf_architecture(&mut f);
    }
    // MZ PE
    if magic[0] == b'M' && magic[1] == b'Z' {
        return pe_architecture_from_mz(&mut f);
    }
    // Raw COFF (no MZ stub)
    let coff_machine = u16::from_le_bytes([magic[0], magic[1]]);
    if is_coff_machine(coff_machine) {
        return Some(format!("PE {}", pe_machine_to_string(coff_machine)));
    }
    // Mach-O
    let be = u32::from_be_bytes(magic);
    if is_macho_magic(be) {
        return parse_macho(&mut f, be, magic);
    }
    None
}

fn is_coff_machine(m: u16) -> bool {
    matches!(m, 0x14c | 0x8664 | 0x1c0 | 0x1c4 | 0xaa64 | 0x5032 | 0x5064 | 0x5128)
}

fn is_macho_magic(m: u32) -> bool {
    matches!(
        m,
        0xfeedface | 0xfeedfacf | 0xcafebabe | 0xcefaedfe | 0xcffaedfe | 0xbebafeca
    )
}

fn elf_architecture(f: &mut std::fs::File) -> Option<String> {
    // 4 magic bytes already consumed; read the remaining 16 (bytes 4..20)
    let mut rest = [0u8; 16];
    if f.read_exact(&mut rest).is_err() {
        return None;
    }
    let e_ident5 = rest[1]; // file byte offset 5
    let little = e_ident5 == 1;
    let big = e_ident5 == 2;
    if !little && !big {
        return None;
    }
    let m0 = rest[14]; // file byte offset 18
    let m1 = rest[15]; // file byte offset 19
    let machine = if little {
        u16::from_le_bytes([m0, m1])
    } else {
        u16::from_be_bytes([m0, m1])
    };
    Some(format!("ELF {}", elf_machine_to_string(machine)))
}

fn elf_machine_to_string(machine: u16) -> String {
    match machine {
        3 => "i386".to_string(),
        8 => "MIPS".to_string(),
        20 => "PowerPC".to_string(),
        21 => "PowerPC64".to_string(),
        22 => "s390x".to_string(),
        40 => "ARM".to_string(),
        43 => "SPARC64".to_string(),
        62 => "x86-64".to_string(),
        183 => "ARM64".to_string(),
        243 => "RISC-V".to_string(),
        _ => format!("Unknown (0x{:x})", machine),
    }
}

fn pe_architecture_from_mz(f: &mut std::fs::File) -> Option<String> {
    // e_lfanew = u32 LE at offset 0x3c
    if f.seek(SeekFrom::Start(0x3c)).is_err() {
        return None;
    }
    let mut lfanew_bytes = [0u8; 4];
    if f.read_exact(&mut lfanew_bytes).is_err() {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(lfanew_bytes);
    let file_len = f.metadata().ok()?.len();
    if file_len < e_lfanew as u64 + 6 {
        return None;
    }
    // signature at e_lfanew must be b"PE\0\0"
    if f.seek(SeekFrom::Start(e_lfanew as u64)).is_err() {
        return None;
    }
    let mut sig = [0u8; 4];
    if f.read_exact(&mut sig).is_err() {
        return None;
    }
    if sig != [b'P', b'E', 0, 0] {
        return None;
    }
    // machine = u16 LE at e_lfanew + 4 (right after the signature)
    let mut mach_bytes = [0u8; 2];
    if f.read_exact(&mut mach_bytes).is_err() {
        return None;
    }
    let machine = u16::from_le_bytes(mach_bytes);
    Some(format!("PE {}", pe_machine_to_string(machine)))
}

fn pe_machine_to_string(machine: u16) -> String {
    match machine {
        0x14c => "i386".to_string(),
        0x8664 => "x86-64".to_string(),
        0x1c0 => "ARM".to_string(),
        0xaa64 => "ARM64".to_string(),
        _ => format!("Unknown (0x{:x})", machine),
    }
}

fn parse_macho(f: &mut std::fs::File, be: u32, magic: [u8; 4]) -> Option<String> {
    let le = u32::from_le_bytes(magic);
    if be == 0xcafebabe {
        return macho_fat_architecture(f);
    }
    if (be & !1) == 0xfeedface {
        return macho_thin_architecture(f, true, be);
    }
    if (le & !1) == 0xfeedface {
        return macho_thin_architecture(f, false, le);
    }
    None
}

fn macho_thin_architecture(f: &mut std::fs::File, big_endian: bool, magic_val: u32) -> Option<String> {
    let is_64 = magic_val == 0xfeedfacf || magic_val == 0xcffaedfe;
    let min_len = if is_64 { 32 } else { 28 };
    let file_len = f.metadata().ok()?.len();
    if file_len < min_len as u64 {
        return None;
    }
    if f.seek(SeekFrom::Start(4)).is_err() {
        return None;
    }
    let mut cpu_bytes = [0u8; 4];
    if f.read_exact(&mut cpu_bytes).is_err() {
        return None;
    }
    let cpu = if big_endian {
        u32::from_be_bytes(cpu_bytes)
    } else {
        u32::from_le_bytes(cpu_bytes)
    };
    Some(format!("Mach-O {}", macho_cpu_to_string(cpu)))
}

fn macho_fat_architecture(f: &mut std::fs::File) -> Option<String> {
    // narch = u32 BE at offset 4, must be >= 1
    if f.seek(SeekFrom::Start(4)).is_err() {
        return None;
    }
    let mut narch_bytes = [0u8; 4];
    if f.read_exact(&mut narch_bytes).is_err() {
        return None;
    }
    let narch = u32::from_be_bytes(narch_bytes);
    if narch < 1 {
        return None;
    }
    let file_len = f.metadata().ok()?.len();
    let mut archs: Vec<String> = Vec::new();
    for i in 0..narch {
        if file_len < 8 + 20 * (i as u64 + 1) {
            return None;
        }
        let offset = 8 + 20 * i as u64;
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return None;
        }
        let mut cpu_bytes = [0u8; 4];
        if f.read_exact(&mut cpu_bytes).is_err() {
            return None;
        }
        let cpu = u32::from_be_bytes(cpu_bytes);
        archs.push(macho_cpu_to_string(cpu));
    }
    if archs.len() == 1 {
        Some(format!("Mach-O {}", archs[0]))
    } else {
        Some(format!("Mach-O Universal ({})", archs.join(", ")))
    }
}

fn macho_cpu_to_string(cpu: u32) -> String {
    match cpu {
        7 => "i386".to_string(),
        0x01000007 => "x86-64".to_string(),
        12 => "ARM".to_string(),
        0x0100000c => "ARM64".to_string(),
        18 => "PowerPC".to_string(),
        0x01000012 => "PowerPC64".to_string(),
        _ => format!("Unknown (0x{:x})", cpu),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("spfmd_test_{}", std::process::id()))
    }

    #[test]
    fn test_sort_priority() {
        let mut rows = vec![
            ("Group".to_string(), "g".to_string()),
            ("Name".to_string(), "n".to_string()),
            ("MD5Checksum".to_string(), "m".to_string()),
            ("Size".to_string(), "s".to_string()),
            ("Date Modified".to_string(), "dm".to_string()),
            ("Permissions".to_string(), "p".to_string()),
            ("SomeExifKey".to_string(), "e1".to_string()),
            ("OtherExifKey".to_string(), "e2".to_string()),
            ("Owner".to_string(), "o".to_string()),
            ("Path".to_string(), "pa".to_string()),
            ("Architecture".to_string(), "a".to_string()),
        ];
        sort_rows(&mut rows);
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "Name",
                "Size",
                "Date Modified",
                "Permissions",
                "Owner",
                "Group",
                "Path",
                "Architecture",
                "MD5Checksum",
                "OtherExifKey",
                "SomeExifKey"
            ]
        );
    }

    #[test]
    fn test_move_render_wrap() {
        let mut m = Metadata::new();
        let data = MetadataData {
            filepath: "/x".to_string(),
            rows: vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ],
            info_msg: String::new(),
        };
        m.set_metadata(data, false);
        assert_eq!(m.render_index, 0);
        // list_up from 0 with 3 rows → (0 - 1) rem_euclid 3 = 2
        m.list_up();
        assert_eq!(m.render_index, 2);
        // reset to 0, then list_down → 1
        m.reset_render();
        m.list_down();
        assert_eq!(m.render_index, 1);
        // len 0 → no panic
        let mut empty = Metadata::new();
        empty.list_up();
        empty.list_down();
    }

    #[test]
    fn test_compute_metadata_widths() {
        assert_eq!(compute_metadata_widths(50, 10), (10, 40));
        assert_eq!(compute_metadata_widths(20, 15), (10, 10));
        // view_width=10, max_key_len=10 → key=10, value=0 → 0 < 5 → value=5, key=5
        assert_eq!(compute_metadata_widths(10, 10), (5, 5));
    }

    #[test]
    fn test_mode_string() {
        assert_eq!(mode_string(0o100644), "-rw-r--r--");
        assert_eq!(mode_string(0o040755), "drwxr-xr-x");
        assert_eq!(mode_string(0o1004755), "-rwsr-xr-x");
        // setgid directory: dir type + setgid bit, perm 2755
        assert_eq!(mode_string(0o042755), "drwxr-sr-x");
        // regular file + sticky bit, perm 667
        assert_eq!(mode_string(0o101667), "-rw-rw-rwt");
        // directory + sticky bit, perm 554
        assert_eq!(mode_string(0o041554), "dr-xr-xr-T");
        assert_eq!(mode_string(0o120777), "lrwxrwxrwx");
    }

    #[test]
    fn test_is_blank() {
        let m = Metadata::new();
        assert!(m.is_blank());
        let mut m = Metadata::new();
        m.set_blank();
        assert!(!m.is_blank()); // info_msg set
        let mut m = Metadata::new();
        let data = MetadataData {
            filepath: "/x".to_string(),
            rows: vec![("a".to_string(), "1".to_string())],
            info_msg: String::new(),
        };
        m.set_metadata(data, false);
        assert!(!m.is_blank()); // rows non-empty, info_msg empty
    }

    #[test]
    fn test_dir_size() {
        let base = temp_dir().join("dirsize");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let dir = base.join("main");
        std::fs::create_dir_all(&dir).unwrap();

        // nested file of known size: 11 bytes
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("f.txt"), b"hello world").unwrap();

        // a regular file in dir: 6 bytes
        std::fs::write(dir.join("target.txt"), b"target").unwrap();

        // symlink to a file: counts its own length (length of target path)
        let link = dir.join("link");
        std::os::unix::fs::symlink(dir.join("target.txt"), &link).unwrap();
        let link_len = std::fs::symlink_metadata(&link).unwrap().len();

        // symlinked dir: must NOT be followed
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("big.bin"), vec![0u8; 1000]).unwrap();
        let dirlink = dir.join("dirlink");
        std::os::unix::fs::symlink(&outside, &dirlink).unwrap();
        let dirlink_len = std::fs::symlink_metadata(&dirlink).unwrap().len();

        // total = 11 (f.txt) + 6 (target.txt) + link_len + dirlink_len
        // (NOT including the 1000 bytes inside the symlinked dir)
        let expected = 11u64 + 6 + link_len + dirlink_len;
        assert_eq!(dir_size(&dir), expected);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_binary_architecture() {
        let dir = temp_dir().join("arch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // (a) ELF x86-64: 20-byte header
        let mut elf = vec![0u8; 20];
        elf[0] = 0x7f;
        elf[1] = b'E';
        elf[2] = b'L';
        elf[3] = b'F';
        elf[4] = 2; // ELFCLASS64
        elf[5] = 1; // ELFDATA2LSB (little-endian)
        elf[6] = 1; // EV_CURRENT
        elf[7] = 0;
        elf[18] = 62; // e_machine = EM_X86_64, little-endian
        elf[19] = 0;
        let elf_path = dir.join("elf");
        std::fs::write(&elf_path, &elf).unwrap();
        assert_eq!(
            binary_architecture(&elf_path).as_deref(),
            Some("ELF x86-64")
        );

        // (b) PE x86-64: MZ stub + e_lfanew=0x40 + "PE\0\0" + machine 0x8664
        let mut pe = vec![0u8; 0x46]; // 70 bytes
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[0x3c] = 0x40; // e_lfanew u32 LE = 0x40
        pe[0x40] = b'P';
        pe[0x40 + 1] = b'E';
        pe[0x40 + 2] = 0;
        pe[0x40 + 3] = 0;
        pe[0x44] = 0x64; // machine 0x8664 LE
        pe[0x44 + 1] = 0x86;
        let pe_path = dir.join("pe");
        std::fs::write(&pe_path, &pe).unwrap();
        assert_eq!(
            binary_architecture(&pe_path).as_deref(),
            Some("PE x86-64")
        );

        // (c) 8 zero bytes → None
        let zero_path = dir.join("zero");
        std::fs::write(&zero_path, [0u8; 8]).unwrap();
        assert_eq!(binary_architecture(&zero_path), None);

        // (d) Mach-O thin LE: CIGAM32 magic + cpu 7 (i386) LE, padded to >= 28
        let mut macho = vec![0u8; 28];
        macho[0] = 0xce;
        macho[1] = 0xfa;
        macho[2] = 0xed;
        macho[3] = 0xfe;
        macho[4] = 7; // cpu = i386, little-endian u32
        macho[5] = 0;
        macho[6] = 0;
        macho[7] = 0;
        let macho_path = dir.join("macho");
        std::fs::write(&macho_path, &macho).unwrap();
        assert_eq!(
            binary_architecture(&macho_path).as_deref(),
            Some("Mach-O i386")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
