//! Small shared helpers: string truncation, size/time formatting, shell exec.

use std::path::Path;
use std::process::{Command, Stdio};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn str_width(s: &str) -> usize {
    s.width()
}

/// Approximation of Go's `unicode.IsGraphic` (categories L, M, N, P, S, Zs),
/// using std-only char classification.
pub fn is_graphic(r: char) -> bool {
    let c = r as u32;
    // C0 and C1 controls
    if c < 0x20 || (0x7f..0xa0).contains(&c) {
        return false;
    }
    // noncharacters
    if matches!(c, 0xfdd0..=0xfdef | 0xfffe | 0xffff) {
        return false;
    }
    // line/paragraph separators (Zl/Zp)
    if c == 0x2028 || c == 0x2029 {
        return false;
    }
    // common Cf (format) code points
    if matches!(
        c,
        0xad
            | 0x200b
            | 0x200c
            | 0x200d
            | 0x200e
            | 0x200f
            | 0x202a
            | 0x202b
            | 0x202c
            | 0x202d
            | 0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x206f
            | 0xfe00..=0xfe0f
            | 0xfeff
            | 0xfff9..=0xfffb
            | 0xe0000..=0xe007f
    ) {
        return false;
    }
    true
}

/// Plain truncation to at most `max` display columns, no tails appended.
pub fn plain_truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if str_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(1);
        if cw == 0 {
            out.push(c);
            continue;
        }
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Go `common.TruncateText` (exact, including its quirk): first truncate to
/// `max - len(tails)`; only append `tails` when truncation actually happened.
/// "Hello" with max=5 → "He..." (width 5), "Hello" with max=10 → "Hello".
pub fn truncate_end(s: &str, max: usize, tails: &str) -> String {
    let t = plain_truncate(s, max.saturating_sub(tails.chars().count()));
    if t.as_str() == s {
        s.to_string()
    } else {
        format!("{t}{tails}")
    }
}

/// Truncate `s` to at most `max` display columns, appending an ellipsis.
/// (Convenience wrapper around the Go-exact version.)
pub fn truncate_end_ellipsis(s: &str, max: usize) -> String {
    truncate_end(s, max, "...")
}

/// Go `common.TruncateTextBeginning` (exact): drop leading runes while the
/// width exceeds `max`, then (if the remainder is longer than the tails)
/// prepend the tails and drop that many leading runes again.
pub fn truncate_beginning(s: &str, max: usize, tails: &str) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    let mut runes: Vec<char> = s.chars().collect();
    let mut width = str_width(s);
    while width > max && !runes.is_empty() {
        let r = runes.remove(0);
        width = width.saturating_sub(r.width().unwrap_or(1));
    }
    let skip = tails.chars().count();
    if runes.len() > skip {
        let rest: String = runes[skip..].iter().collect();
        format!("{tails}{rest}")
    } else {
        runes.iter().collect()
    }
}

/// Truncate `s` from the *beginning*, keeping the tail. Prefixes with `...`.
/// (Convenience wrapper around the Go-exact version.)
pub fn truncate_beginning_ellipsis(s: &str, max: usize) -> String {
    truncate_beginning(s, max, "...")
}

/// Go `common.TruncateMiddleText` (exact, including its byte-slice quirk:
/// head is the first `half` *bytes*, tail starts at rune-count-based offset).
pub fn truncate_middle(s: &str, max: usize, tails: &str) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let half = (max - 3) / 2;
    let mut head_end = half.min(s.len());
    if !s.is_char_boundary(head_end) {
        while head_end > 0 && !s.is_char_boundary(head_end) {
            head_end -= 1;
        }
    }
    let mut tail_start = (n - half).min(s.len());
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{}{}", &s[..head_end], tails, &s[tail_start..])
}

/// Go `common.FormatFileSize` (exact: log-based unit index, "%d B" for the
/// B unit, "%.2f %s" otherwise).
pub fn format_file_size(bytes: u64, si: bool) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let units: &[&str] = if si {
        &["B", "kB", "MB", "GB", "TB", "PB", "EB"]
    } else {
        &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]
    };
    let power: f64 = if si { 1000.0 } else { 1024.0 };
    let idx = ((bytes as f64).ln() / power.ln()).floor() as usize;
    if idx == 0 {
        return format!("{bytes} B");
    }
    let adj = bytes as f64 / power.powi(idx.min(6) as i32);
    format!("{adj:.2} {}", units[idx.min(6)])
}

/// Format a modification time as `YYYY-MM-DD HH:MM`.
pub fn format_mtime(ts: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = match ts.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
            .map(|t| t.into())
            .unwrap_or_else(chrono::Local::now),
        Err(_) => chrono::Local::now(),
    };
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// Unix-style permission string like `-rw-r--r--`.
pub fn format_permissions(path: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = path.metadata() else {
        return "?????????".to_string();
    };
    let mode = meta.permissions().mode();
    let mut s = String::with_capacity(10);
    s.push(match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o010000 => 'c',
        0o020000 => 'b',
        0o060000 => 's',
        _ => '-',
    });
    for shift in [6u32, 3, 0] {
        let p = (mode >> shift) & 0o7;
        s.push(if p & 4 != 0 { 'r' } else { '-' });
        s.push(if p & 2 != 0 { 'w' } else { '-' });
        s.push(if p & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Go `common.IsTextFile` (exact): read up to 1024 bytes, the buffer must be
/// printable UTF-8 (valid runes, no U+FFFD, only printable/space runes,
/// BOM allowed). A truncated multi-byte rune at the buffer end is only
/// accepted when the file continues past it.
pub fn is_text_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    loop {
        if buf.len() >= 1024 {
            break;
        }
        let want = (1024 - buf.len()).min(tmp.len());
        match f.read(&mut tmp[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    // Did the file end exactly at our buffer end?
    let at_eof = if buf.len() == 1024 {
        let mut e = [0u8; 1];
        matches!(f.read(&mut e), Ok(0))
    } else {
        true
    };
    buffer_printable(&buf, at_eof)
}

/// Go `common.IsBufferPrintable` (exact).
pub fn buffer_printable(buf: &[u8], at_eof: bool) -> bool {
    let mut i = 0usize;
    while i < buf.len() {
        let Some(n) = utf8_rune_len(buf[i]) else {
            return false;
        };
        if i + n > buf.len() {
            // Incomplete rune at the end: allowed only when more data follows
            return !at_eof;
        }
        let Ok(r) = std::str::from_utf8(&buf[i..i + n]) else {
            return false;
        };
        let r = r.chars().next().unwrap();
        if r != '\u{feff}' && !is_graphic(r) && !r.is_whitespace() {
            return false;
        }
        i += n;
    }
    true
}

fn utf8_rune_len(b: u8) -> Option<usize> {
    if b < 0x80 {
        Some(1)
    } else if b & 0xe0 == 0xc0 {
        Some(2)
    } else if b & 0xf0 == 0xe0 {
        Some(3)
    } else if b & 0xf8 == 0xf0 {
        Some(4)
    } else {
        None
    }
}

/// Go `common.MakePrintable` (exact, with esc passthrough): NBSP kept, ESC
/// passed through, wide (>0x7f) multi-byte spaces become ' ', ASCII is kept
/// only when graphic (plus '\n'), tabs expand to the next multiple-of-4
/// column.
pub fn make_printable(line: &str) -> String {
    let mut sb = String::new();
    // byte offset where the current tab-stop segment starts
    let mut last_segment_start = 0usize;
    for r in line.chars() {
        if r == '\u{fffd}' {
            continue;
        }
        if r == '\u{a0}' {
            sb.push(r);
            continue;
        }
        if r == '\t' {
            let seg = &sb[last_segment_start..];
            sb.push_str(&" ".repeat(4 - str_width(seg) % 4));
            last_segment_start = sb.len();
            continue;
        }
        if r == '\x1b' {
            sb.push(r);
            continue;
        }
        if (r as u32) > 0x7f {
            if r.is_whitespace() && r.len_utf8() > 1 {
                sb.push(' ');
            } else {
                sb.push(r);
            }
            continue;
        }
        if r == '\n' {
            sb.push(r);
            last_segment_start = sb.len();
        }
        if is_graphic(r) {
            sb.push(r);
        }
    }
    sb
}

/// Run a command line through the user's shell (`sh -c`), capturing combined output.
pub fn run_shell(cmdline: &str, cwd: &Path) -> (String, bool) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmdline)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            (text, o.status.success())
        }
        Err(e) => (format!("failed to run shell: {e}"), false),
    }
}

/// Alphanumeric ("natural") comparison: `file2` < `file10`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let ca: Vec<char> = a.chars().collect();
    let cb: Vec<char> = b.chars().collect();
    let (mut ia, mut ib) = (0usize, 0usize);
    while ia < ca.len() && ib < cb.len() {
        if ca[ia].is_ascii_digit() && cb[ib].is_ascii_digit() {
            let (mut na, mut nb) = (ia, ib);
            while na < ca.len() && ca[na].is_ascii_digit() {
                na += 1;
            }
            while nb < cb.len() && cb[nb].is_ascii_digit() {
                nb += 1;
            }
            let sa: &str = a.get(ia..na).unwrap_or("");
            let sb: &str = b.get(ib..nb).unwrap_or("");
            // strip leading zeros for numeric compare, fallback to lexicographic
            let (va, vb) = (sa.trim_start_matches('0'), sb.trim_start_matches('0'));
            let ord = if va.len() != vb.len() {
                va.len().cmp(&vb.len())
            } else {
                va.cmp(vb)
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            ia = na;
            ib = nb;
        } else {
            let ord = ca[ia].cmp(&cb[ib]);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            ia += 1;
            ib += 1;
        }
    }
    ca.len().cmp(&cb.len())
}

/// Percent-encode a path for `.trashinfo` files (RFC 3986, keep unreserved + '/' '.' '-').
pub fn url_escape_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        let ok = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/');
        if ok {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
