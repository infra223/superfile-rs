//! Icons and glyphs, ported 1:1 from superfile's Go `config/icon` package
//! (`icon.go`, `function.go`) and the lookup logic of
//! `internal/common/icon_utils.go`. The icon tables are based on
//! https://github.com/acarl005/ls-go (as credited in the Go source).
//!
//! Go's `Style{Icon, Color}` with the sentinel `Color == "NONE"` maps to
//! `IconStyle { glyph, color: None }`: `None` means "no color set", so the
//! caller uses its component's default foreground (Go renders `NONE` entries
//! with `Theme.FilePanelFG`).

use ratatui::style::Color;

/// Style for a single icon glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconStyle {
    pub glyph: &'static str,
    /// None → the caller uses its component's default foreground.
    pub color: Option<Color>,
}

/// All UI glyphs for the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ui {
    pub nerdfont: bool,
    pub space: &'static str,
    pub superfile: &'static str,
    pub home: &'static str,
    pub desktop: &'static str,
    pub download: &'static str,
    pub documents: &'static str,
    pub pictures: &'static str,
    pub videos: &'static str,
    pub music: &'static str,
    pub templates: &'static str,
    pub public_share: &'static str,
    pub trash: &'static str,
    pub compress_file: &'static str,
    pub extract_file: &'static str,
    pub copy: &'static str,
    pub cut: &'static str,
    pub delete: &'static str,
    pub cursor: &'static str,
    pub browser: &'static str,
    pub select: &'static str,
    pub checkbox_empty: &'static str,
    pub checkbox_checked: &'static str,
    pub error: &'static str,
    pub warn: &'static str,
    pub done: &'static str,
    pub in_operation: &'static str,
    pub directory: &'static str,
    pub search: &'static str,
    pub sort_asc: &'static str,
    pub sort_desc: &'static str,
    pub terminal: &'static str,
    pub pinned: &'static str,
    pub disk: &'static str,
}

/// Build the glyph set for the given mode (nerdfont on/off).
///
/// The non-nerdfont branch is an exact port of Go's `InitIcon(false, ...)`:
/// the ASCII-only fallbacks listed below are applied, and — a verified Go
/// quirk — `desktop`, `trash` and both checkbox glyphs are NOT in the swap
/// list, so they keep their nerd-font glyphs even without nerdfont enabled.
pub fn ui_icons(nerdfont: bool) -> Ui {
    if nerdfont {
        Ui {
            nerdfont: true,
            space: " ",
            superfile: "\u{e6ad}",
            home: "\u{f02dc}",
            desktop: "\u{f01c4}",
            download: "\u{f03d4}",
            documents: "\u{f0219}",
            pictures: "\u{f02e9}",
            videos: "\u{f0381}",
            music: "♬",
            templates: "\u{f03e2}",
            public_share: "\u{f0ac}",
            trash: "\u{f1f8}",
            compress_file: "\u{f05c4}",
            extract_file: "\u{f06eb}",
            copy: "\u{f018f}",
            cut: "\u{f0190}",
            delete: "\u{f01b4}",
            cursor: "\u{f054}",
            browser: "\u{f0208}",
            select: "\u{f01bd}",
            checkbox_empty: "\u{f0131}",
            checkbox_checked: "\u{f0856}",
            error: "\u{f530}",
            warn: "\u{f071}",
            done: "\u{f4a4}",
            in_operation: "\u{f0954}",
            directory: "\u{f07b}",
            search: "\u{e68f}",
            sort_asc: "\u{f0de}",
            sort_desc: "\u{f0dd}",
            terminal: "\u{e795}",
            pinned: "\u{f0403}",
            disk: "\u{f11f0}",
        }
    } else {
        Ui {
            nerdfont: false,
            space: "",
            superfile: "",
            home: "",
            // Kept: InitIcon does not swap Desktop in non-nerdfont mode.
            desktop: "\u{f01c4}",
            download: "",
            documents: "",
            pictures: "",
            videos: "",
            music: "",
            templates: "",
            public_share: "",
            // Kept: InitIcon does not swap Trash in non-nerdfont mode.
            trash: "\u{f1f8}",
            compress_file: "",
            extract_file: "",
            copy: "",
            cut: "",
            delete: "",
            cursor: ">",
            browser: "B",
            select: "S",
            // Kept: InitIcon does not touch the checkbox glyphs.
            checkbox_empty: "\u{f0131}",
            checkbox_checked: "\u{f0856}",
            error: "",
            warn: "",
            done: "",
            in_operation: "",
            directory: "",
            search: "",
            sort_asc: "^",
            sort_desc: "v",
            terminal: "",
            pinned: "",
            disk: "",
        }
    }
}

/// Generic file icon, Go `Icons["file"]` (color "NONE" → `None`).
const ICON_FILE: (&'static str, Option<Color>) = ("\u{f15b}", None);
/// Symlinked file, Go `Icons["link_file"]` (color "NONE" → `None`).
const ICON_LINK_FILE: (&'static str, Option<Color>) = ("\u{f481}", None);
/// Symlinked directory, Go `Folders["link_folder"]` (color "NONE" → `None`).
const ICON_LINK_FOLDER: (&'static str, Option<Color>) = ("\u{f482}", None);
/// Generic directory glyph, Go `Folders["folder"]` set by `InitIcon`.
const GLYPH_FOLDER: &str = "\u{f07b}";

/// Replicate Go's `path/filepath.Ext`: the suffix beginning at the final dot
/// in the final path element (i.e. after the last '/'); empty when there is
/// no dot in the final element. A dot at the first byte of the path yields
/// the whole path.
fn go_ext(path: &str) -> &str {
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        match b[i] {
            b'/' => return "",
            b'.' => return &path[i..],
            _ => {}
        }
    }
    ""
}

/// Go's `strings.TrimPrefix(filepath.Ext(name), ".")`.
fn file_ext(name: &str) -> &str {
    go_ext(name).strip_prefix('.').unwrap_or("")
}

/// Go `Aliases` map: `map[string]string`, keys and values are lower-cased
/// names/extensions. Lookups happen on lower-cased keys.
fn alias_target(key: &str) -> Option<&'static str> {
    match key {
        "dart" => Some("dart"),
        "apk" => Some("android"),
        "gradle" => Some("android"),
        "ds_store" => Some("apple"),
        "localized" => Some("apple"),
        "m" => Some("apple"),
        "mm" => Some("apple"),
        "s" => Some("asm"),
        "aac" => Some("audio"),
        "alac" => Some("audio"),
        "flac" => Some("audio"),
        "m4a" => Some("audio"),
        "mka" => Some("audio"),
        "mp3" => Some("audio"),
        "ogg" => Some("audio"),
        "opus" => Some("audio"),
        "wav" => Some("audio"),
        "wma" => Some("audio"),
        "bson" => Some("binary"),
        "feather" => Some("binary"),
        "mat" => Some("binary"),
        "o" => Some("binary"),
        "pb" => Some("binary"),
        "pickle" => Some("binary"),
        "pkl" => Some("binary"),
        "tfrecord" => Some("binary"),
        "conf" => Some("cfg"),
        "config" => Some("cfg"),
        "cljc" => Some("clj"),
        "cljs" => Some("clj"),
        "editorconfig" => Some("conf"),
        "rc" => Some("conf"),
        "c++" => Some("cpp"),
        "cc" => Some("cpp"),
        "cxx" => Some("cpp"),
        "scss" => Some("css"),
        "sql" => Some("db"),
        "docx" => Some("doc"),
        "gdoc" => Some("doc"),
        "dockerignore" => Some("dockerfile"),
        "epub" => Some("ebook"),
        "ipynb" => Some("ebook"),
        "mobi" => Some("ebook"),
        "env" => Some("env"),
        ".env.local" => Some("env"),
        "local" => Some("env"),
        "f03" => Some("f"),
        "f77" => Some("f"),
        "f90" => Some("f"),
        "f95" => Some("f"),
        "for" => Some("f"),
        "fpp" => Some("f"),
        "ftn" => Some("f"),
        "eot" => Some("font"),
        "otf" => Some("font"),
        "ttf" => Some("font"),
        "woff" => Some("font"),
        "woff2" => Some("font"),
        "fsi" => Some("fs"),
        "fsscript" => Some("fs"),
        "fsx" => Some("fs"),
        "dna" => Some("gb"),
        "gitattributes" => Some("git"),
        "gitconfig" => Some("git"),
        "gitignore" => Some("git"),
        "gitignore_global" => Some("git"),
        "gitmirrorall" => Some("git"),
        "gitmodules" => Some("git"),
        "gltf" => Some("glp"),
        "gsh" => Some("groovy"),
        "gvy" => Some("groovy"),
        "gy" => Some("groovy"),
        "h++" => Some("h"),
        "hh" => Some("h"),
        "hpp" => Some("h"),
        "hxx" => Some("h"),
        "lhs" => Some("hs"),
        "htm" => Some("html"),
        "xhtml" => Some("html"),
        "bmp" => Some("image"),
        "cbr" => Some("image"),
        "cbz" => Some("image"),
        "dvi" => Some("image"),
        "eps" => Some("image"),
        "gif" => Some("image"),
        "ico" => Some("image"),
        "jpeg" => Some("image"),
        "jpg" => Some("image"),
        "nef" => Some("image"),
        "orf" => Some("image"),
        "pbm" => Some("image"),
        "pgm" => Some("image"),
        "png" => Some("image"),
        "pnm" => Some("image"),
        "ppm" => Some("image"),
        "pxm" => Some("image"),
        "sixel" => Some("image"),
        "stl" => Some("image"),
        "svg" => Some("image"),
        "tif" => Some("image"),
        "tiff" => Some("image"),
        "webp" => Some("image"),
        "xpm" => Some("image"),
        "disk" => Some("iso"),
        "dmg" => Some("iso"),
        "img" => Some("iso"),
        "ipsw" => Some("iso"),
        "smi" => Some("iso"),
        "vhd" => Some("iso"),
        "vhdx" => Some("iso"),
        "vmdk" => Some("iso"),
        "jar" => Some("java"),
        "kts" => Some("kt"),
        "cjs" => Some("js"),
        "properties" => Some("json"),
        "webmanifest" => Some("json"),
        "tsx" => Some("jsx"),
        "cjsx" => Some("jsx"),
        "cer" => Some("key"),
        "crt" => Some("key"),
        "der" => Some("key"),
        "gpg" => Some("key"),
        "p7b" => Some("key"),
        "pem" => Some("key"),
        "pfx" => Some("key"),
        "pgp" => Some("key"),
        "license" => Some("key"),
        "codeowners" => Some("maintainers"),
        "credits" => Some("maintainers"),
        "cmake" => Some("makefile"),
        "justfile" => Some("makefile"),
        "markdown" => Some("md"),
        "mkd" => Some("md"),
        "rdoc" => Some("md"),
        "readme" => Some("md"),
        "mli" => Some("ml"),
        "sml" => Some("ml"),
        "netcdf" => Some("nc"),
        "brewfile" => Some("package"),
        "cargo.toml" => Some("package"),
        "cargo.lock" => Some("package"),
        "go.mod" => Some("package"),
        "go.sum" => Some("package"),
        "pyproject.toml" => Some("package"),
        "poetry.lock" => Some("package"),
        "package.json" => Some("package"),
        "pipfile" => Some("package"),
        "pipfile.lock" => Some("package"),
        "php3" => Some("php"),
        "php4" => Some("php"),
        "php5" => Some("php"),
        "phpt" => Some("php"),
        "phtml" => Some("php"),
        "gslides" => Some("ppt"),
        "pptx" => Some("ppt"),
        "pxd" => Some("py"),
        "pyc" => Some("py"),
        "pyx" => Some("py"),
        "whl" => Some("py"),
        "rdata" => Some("r"),
        "rds" => Some("r"),
        "rmd" => Some("r"),
        "gemfile" => Some("rb"),
        "gemspec" => Some("rb"),
        "guardfile" => Some("rb"),
        "procfile" => Some("rb"),
        "rakefile" => Some("rb"),
        "rspec" => Some("rb"),
        "rspec_parallel" => Some("rb"),
        "rspec_status" => Some("rb"),
        "ru" => Some("rb"),
        "erb" => Some("rubydoc"),
        "slim" => Some("rubydoc"),
        "awk" => Some("shell"),
        "bash" => Some("shell"),
        "bash_history" => Some("shell"),
        "bash_profile" => Some("shell"),
        "bashrc" => Some("shell"),
        "csh" => Some("shell"),
        "fish" => Some("shell"),
        "ksh" => Some("shell"),
        "sh" => Some("shell"),
        "zsh" => Some("shell"),
        "zsh-theme" => Some("shell"),
        "zshrc" => Some("shell"),
        "plpgsql" => Some("sql"),
        "plsql" => Some("sql"),
        "psql" => Some("sql"),
        "tsql" => Some("sql"),
        "sl3" => Some("sqlite"),
        "sqlite3" => Some("sqlite"),
        "stylus" => Some("styl"),
        "cls" => Some("tex"),
        "avi" => Some("video"),
        "flv" => Some("video"),
        "m2v" => Some("video"),
        "mkv" => Some("video"),
        "mov" => Some("video"),
        "mp4" => Some("video"),
        "mpeg" => Some("video"),
        "mpg" => Some("video"),
        "ogm" => Some("video"),
        "ogv" => Some("video"),
        "vob" => Some("video"),
        "webm" => Some("video"),
        "vimrc" => Some("vim"),
        "bat" => Some("windows"),
        "cmd" => Some("windows"),
        "exe" => Some("windows"),
        "csv" => Some("xls"),
        "gsheet" => Some("xls"),
        "xlsx" => Some("xls"),
        "plist" => Some("xml"),
        "xul" => Some("xml"),
        "yaml" => Some("yml"),
        "7z" => Some("zip"),
        // Never matched (lookups are lower-cased first), but kept for
        // exact transcription of the Go map.
        "Z" => Some("zip"),
        "bz2" => Some("zip"),
        "gz" => Some("zip"),
        "lzma" => Some("zip"),
        "par" => Some("zip"),
        "rar" => Some("zip"),
        "tar" => Some("zip"),
        "tc" => Some("zip"),
        "tgz" => Some("zip"),
        "txz" => Some("zip"),
        "xz" => Some("zip"),
        "z" => Some("zip"),
        _ => None,
    }
}

/// Go `Icons` map: extension / well-known name → (glyph, color).
/// Color "NONE" in Go → `None`.
fn icon_by_key(key: &str) -> Option<(&'static str, Option<Color>)> {
    match key {
        "ai" => Some(("\u{e669}", Some(Color::Rgb(0xce, 0x6f, 0x14)))),
        "android" => Some(("\u{f17b}", Some(Color::Rgb(0xa7, 0xc8, 0x3f)))),
        "apple" => Some(("\u{e711}", Some(Color::Rgb(0x78, 0x90, 0x9c)))),
        "asm" => Some(("\u{f061a}", Some(Color::Rgb(0xff, 0x78, 0x44)))),
        "audio" => Some(("\u{f001}", Some(Color::Rgb(0xee, 0x52, 0x4f)))),
        "binary" => Some(("\u{f471}", Some(Color::Rgb(0xff, 0x78, 0x44)))),
        "c" => Some(("\u{e649}", Some(Color::Rgb(0x01, 0x88, 0xd2)))),
        "cfg" => Some(("\u{e615}", Some(Color::Rgb(0x8b, 0x8b, 0x8b)))),
        "clj" => Some(("\u{e76a}", Some(Color::Rgb(0x68, 0xb3, 0x38)))),
        "conf" => Some(("\u{e615}", Some(Color::Rgb(0x8b, 0x8b, 0x8b)))),
        "cpp" => Some(("\u{e646}", Some(Color::Rgb(0x01, 0x88, 0xd2)))),
        "css" => Some(("\u{f13c}", Some(Color::Rgb(0x2d, 0x53, 0xe5)))),
        "dart" => Some(("\u{e64c}", Some(Color::Rgb(0x03, 0x58, 0x9b)))),
        "db" => Some(("\u{f1c0}", Some(Color::Rgb(0xff, 0x84, 0x00)))),
        "deb" => Some(("\u{e77d}", Some(Color::Rgb(0xab, 0x08, 0x36)))),
        "doc" => Some(("\u{e6a5}", Some(Color::Rgb(0x29, 0x53, 0x94)))),
        "dockerfile" => Some(("\u{f0868}", Some(Color::Rgb(0x09, 0x9c, 0xec)))),
        "ebook" => Some(("\u{f02d}", Some(Color::Rgb(0x67, 0xb5, 0x00)))),
        "env" => Some(("\u{f462}", Some(Color::Rgb(0xee, 0xd6, 0x45)))),
        "f" => Some(("\u{f121a}", Some(Color::Rgb(0x8e, 0x44, 0xad)))),
        "file" => Some(("\u{f15b}", None)),
        "font" => Some(("\u{f031}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "fs" => Some(("\u{e7a7}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "gb" => Some(("\u{e272}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "gform" => Some(("\u{f298}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "git" => Some(("\u{e702}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "go" => Some(("\u{e627}", Some(Color::Rgb(0x6e, 0xd8, 0xe5)))),
        "graphql" => Some(("\u{e662}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "glp" => Some(("\u{f01a7}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "groovy" => Some(("\u{e775}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "gruntfile.js" => Some(("\u{e74c}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "gulpfile.js" => Some(("\u{e610}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "gv" => Some(("\u{e225}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "h" => Some(("\u{f0fd}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "haml" => Some(("\u{e664}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "hs" => Some(("\u{e777}", Some(Color::Rgb(0x29, 0x80, 0xb9)))),
        "html" => Some(("\u{f13b}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "hx" => Some(("\u{e666}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "ics" => Some(("\u{f073}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "image" => Some(("\u{f1c5}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "iml" => Some(("\u{e7b5}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "ini" => Some(("\u{f016a}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "ino" => Some(("\u{e255}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "iso" => Some(("\u{f02ca}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "jade" => Some(("\u{e66c}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "java" => Some(("\u{e738}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "jenkinsfile" => Some(("\u{e767}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "jl" => Some(("\u{e624}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "js" => Some(("\u{e781}", Some(Color::Rgb(0xf3, 0x9c, 0x12)))),
        "json" => Some(("\u{e60b}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "jsx" => Some(("\u{e7ba}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "key" => Some(("\u{f43d}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "ko" => Some(("\u{ebc6}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "kt" => Some(("\u{e634}", Some(Color::Rgb(0x29, 0x80, 0xb9)))),
        "less" => Some(("\u{e758}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "link_file" => Some(("\u{f481}", None)),
        "lock" => Some(("\u{f023}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "log" => Some(("\u{f18d}", Some(Color::Rgb(0x7f, 0x8c, 0x8d)))),
        "lua" => Some(("\u{e620}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "maintainers" => Some(("\u{f0c0}", Some(Color::Rgb(0x7f, 0x8c, 0x8d)))),
        "makefile" => Some(("\u{e20f}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "md" => Some(("\u{f48a}", Some(Color::Rgb(0x7f, 0x8c, 0x8d)))),
        "mjs" => Some(("\u{e718}", Some(Color::Rgb(0xf3, 0x9c, 0x12)))),
        "ml" => Some(("\u{f0627}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "mustache" => Some(("\u{e60f}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        // Go entry is `Color: "#f1c40"` — a malformed 5-digit hex that the
        // Go renderer cannot parse, so no color is applied. Port as None.
        "nc" => Some(("\u{f02c1}", None)),
        "nim" => Some(("\u{e677}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "nix" => Some(("\u{f313}", Some(Color::Rgb(0xf3, 0x9c, 0x12)))),
        "npmignore" => Some(("\u{e71e}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "package" => Some(("\u{f03d7}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "passwd" => Some(("\u{f023}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "patch" => Some(("\u{f440}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "pdf" => Some(("\u{f1c1}", Some(Color::Rgb(0xd3, 0x54, 0x00)))),
        "php" => Some(("\u{e608}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "pl" => Some(("\u{e7a1}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "prisma" => Some(("\u{e684}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "ppt" => Some(("\u{f1c4}", Some(Color::Rgb(0xc0, 0x39, 0x2b)))),
        "ps" => Some(("\u{f1517}", Some(Color::Rgb(0xd3, 0x54, 0x00)))),
        "psd" => Some(("\u{e7b8}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "py" => Some(("\u{e606}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "r" => Some(("\u{e68a}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "rb" => Some(("\u{e21e}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "rdb" => Some(("\u{e76d}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "rpm" => Some(("\u{f17c}", Some(Color::Rgb(0xd3, 0x54, 0x00)))),
        "rs" => Some(("\u{e7a8}", Some(Color::Rgb(0xf3, 0x9c, 0x12)))),
        "rss" => Some(("\u{f09e}", Some(Color::Rgb(0xc0, 0x39, 0x2b)))),
        "rst" => Some(("\u{f016b}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "rubydoc" => Some(("\u{e73b}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "sass" => Some(("\u{e603}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "scala" => Some(("\u{e737}", Some(Color::Rgb(0xe6, 0x7e, 0x22)))),
        "shell" => Some(("\u{f489}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "shp" => Some(("\u{f065e}", Some(Color::Rgb(0xf1, 0xc4, 0x0f)))),
        "sol" => Some(("\u{f086a}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "sqlite" => Some(("\u{e7c4}", Some(Color::Rgb(0x27, 0xae, 0x60)))),
        "styl" => Some(("\u{e600}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        "svelte" => Some(("\u{e697}", Some(Color::Rgb(0xff, 0x3e, 0x00)))),
        "swift" => Some(("\u{e755}", Some(Color::Rgb(0xff, 0x6f, 0x61)))),
        "tex" => Some(("\u{222b}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "tf" => Some(("\u{e69a}", Some(Color::Rgb(0x2e, 0xcc, 0x71)))),
        "toml" => Some(("\u{f016a}", Some(Color::Rgb(0xf3, 0x9c, 0x12)))),
        "ts" => Some(("\u{f06e6}", Some(Color::Rgb(0x29, 0x80, 0xb9)))),
        "twig" => Some(("\u{e61c}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "txt" => Some(("\u{f15c}", Some(Color::Rgb(0x7f, 0x8c, 0x8d)))),
        "vagrantfile" => Some(("\u{e21e}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "video" => Some(("\u{f03d}", Some(Color::Rgb(0xc0, 0x39, 0x2b)))),
        "vim" => Some(("\u{e62b}", Some(Color::Rgb(0x01, 0x98, 0x33)))),
        "vue" => Some(("\u{e6a0}", Some(Color::Rgb(0x41, 0xb8, 0x83)))),
        "windows" => Some(("\u{f17a}", Some(Color::Rgb(0x4a, 0x90, 0xe2)))),
        "xls" => Some(("\u{f1c3}", Some(Color::Rgb(0x27, 0xae, 0x60)))),
        "xml" => Some(("\u{e796}", Some(Color::Rgb(0x34, 0x98, 0xdb)))),
        "yml" => Some(("\u{e601}", Some(Color::Rgb(0xf3, 0x9c, 0x12)))),
        "zig" => Some(("\u{e6a9}", Some(Color::Rgb(0x9b, 0x59, 0xb6)))),
        "zip" => Some(("\u{f410}", Some(Color::Rgb(0xe7, 0x4c, 0x3c)))),
        _ => None,
    }
}

/// Go `Folders` map (static entries). The generic "folder" key is NOT here;
/// it is created at runtime by `InitIcon` with the configured directory icon
/// color (or the `noIconColor` = "NONE" default).
fn folder_by_key(name: &str) -> Option<(&'static str, Option<Color>)> {
    match name {
        ".atom" => Some(("\u{e764}", Some(Color::Rgb(0x66, 0x59, 0x5c)))),
        ".aws" => Some(("\u{e7ad}", Some(Color::Rgb(0xff, 0x99, 0x00)))),
        ".docker" => Some(("\u{e7b0}", Some(Color::Rgb(0x0d, 0xb7, 0xed)))),
        ".gem" => Some(("\u{e21e}", Some(Color::Rgb(0xe9, 0x57, 0x3f)))),
        ".git" => Some(("\u{e5fb}", Some(Color::Rgb(0xf1, 0x4e, 0x32)))),
        ".git-credential-cache" => Some(("\u{e5fb}", Some(Color::Rgb(0xf1, 0x4e, 0x32)))),
        ".github" => Some(("\u{e5fd}", Some(Color::Rgb(0x00, 0x00, 0x00)))),
        ".npm" => Some(("\u{e5fa}", Some(Color::Rgb(0xcb, 0x38, 0x37)))),
        ".nvm" => Some(("\u{e718}", Some(Color::Rgb(0xcb, 0x38, 0x37)))),
        ".rvm" => Some(("\u{e21e}", Some(Color::Rgb(0xe9, 0x57, 0x3f)))),
        ".Trash" => Some(("\u{f1f8}", Some(Color::Rgb(0x7f, 0x8c, 0x8d)))),
        ".vscode" => Some(("\u{e70c}", Some(Color::Rgb(0x00, 0x7a, 0xcc)))),
        ".vim" => Some(("\u{e62b}", Some(Color::Rgb(0x01, 0x98, 0x33)))),
        "config" => Some(("\u{e5fc}", Some(Color::Rgb(0xff, 0xb8, 0x6c)))),
        "hidden" => Some(("\u{f023}", Some(Color::Rgb(0x75, 0x71, 0x5e)))),
        "node_modules" => Some(("\u{e5fa}", Some(Color::Rgb(0xcb, 0x38, 0x37)))),
        "link_folder" => Some(("\u{f482}", None)),
        "superfile" => Some(("\u{f069d}", Some(Color::Rgb(0xff, 0x6f, 0x00)))),
        _ => None,
    }
}

/// Resolve the icon for a file/directory name.
///
/// Exact port of Go's `common.GetElementIcon` + `getFileIcon`:
/// - non-nerdfont → empty glyph, default foreground
///   (`Style{Icon: "", Color: Theme.FilePanelFG}`).
/// - dir + link → `Folders["link_folder"]` (no color).
/// - dir → `Folders[name]` (case-sensitive, no lowercasing), falling back to
///   `Folders["folder"]` whose color is the configured directory icon color,
///   or "NONE" (→ `None`, default foreground) when unset.
/// - file + link → `Icons["link_file"]` (no color).
/// - file → default `Icons["file"]`; then the lower-cased extension
///   (Go `filepath.Ext` minus the leading dot) is resolved through
///   `Aliases` and looked up in `Icons`; then the lower-cased FULL NAME is
///   resolved through `Aliases` and looked up in `Icons`, overriding the
///   extension result when found.
pub fn icon_for(
    name: &str,
    is_dir: bool,
    is_link: bool,
    ui: &Ui,
    dir_icon_color: Option<Color>,
) -> IconStyle {
    if !ui.nerdfont {
        return IconStyle {
            glyph: "",
            color: None,
        };
    }

    if is_dir {
        if is_link {
            return IconStyle {
                glyph: ICON_LINK_FOLDER.0,
                color: ICON_LINK_FOLDER.1,
            };
        }
        return match folder_by_key(name) {
            Some((glyph, color)) => IconStyle { glyph, color },
            // Folders["folder"] from InitIcon: glyph "\uf07b", color is the
            // directory icon color or noIconColor ("NONE" → None).
            None => IconStyle {
                glyph: GLYPH_FOLDER,
                color: dir_icon_color,
            },
        };
    }

    if is_link {
        return IconStyle {
            glyph: ICON_LINK_FILE.0,
            color: ICON_LINK_FILE.1,
        };
    }

    // Extension-based lookup (Go: resultIcon := Icons["file"] ...).
    let lower_ext = file_ext(name).to_lowercase();
    let ext_target: &str = alias_target(&lower_ext).unwrap_or(lower_ext.as_str());
    let mut result = icon_by_key(ext_target).unwrap_or(ICON_FILE);

    // Full-name lookup overrides the extension result when found
    // (Go: fullName := toLower(file); Aliases → Icons).
    let lower_name = name.to_lowercase();
    let full_target: &str = alias_target(&lower_name).unwrap_or(lower_name.as_str());
    if let Some(found) = icon_by_key(full_target) {
        result = found;
    }

    IconStyle {
        glyph: result.0,
        color: result.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- filepath.Ext replication -----------------------------------------

    #[test]
    fn go_ext_matches_filepath_ext() {
        // Suffix from the final dot of the final element, dot included.
        assert_eq!(go_ext("archive.tar.gz"), ".gz");
        assert_eq!(go_ext(".gitignore"), ".gitignore");
        assert_eq!(go_ext("noext"), "");
        assert_eq!(go_ext("a.b.c"), ".c");
        // Only the final element matters.
        assert_eq!(go_ext("foo/bar.tar"), ".tar");
        assert_eq!(go_ext("foo/bar.baz"), ".baz");
        assert_eq!(go_ext("foo/bar"), "");
        // Trailing dot: Go returns "." (last dot at the end).
        assert_eq!(go_ext("file."), ".");
        assert_eq!(go_ext(""), "");
    }

    #[test]
    fn file_ext_strips_leading_dot() {
        // Mirrors `strings.TrimPrefix(filepath.Ext(name), ".")`.
        assert_eq!(file_ext("archive.tar.gz"), "gz");
        assert_eq!(file_ext(".gitignore"), "gitignore");
        assert_eq!(file_ext("noext"), "");
        assert_eq!(file_ext("a.b.c"), "c");
    }

    // -- icon_for fallback paths -------------------------------------------

    #[test]
    fn non_nerdfont_returns_empty_icon() {
        let ui = ui_icons(false);
        assert_eq!(
            icon_for("test.txt", false, false, &ui, None),
            IconStyle {
                glyph: "",
                color: None
            }
        );
    }

    #[test]
    fn dir_miss_falls_back_to_folder_glyph() {
        let ui = ui_icons(true);
        // No color configured (Go: noIconColor "NONE" → default foreground).
        assert_eq!(
            icon_for("mydir", true, false, &ui, None),
            IconStyle {
                glyph: "\u{f07b}",
                color: None
            }
        );
        // Configured directory icon color is used for the generic folder.
        let blue = Color::Rgb(0x11, 0x22, 0x33);
        assert_eq!(
            icon_for("mydir", true, false, &ui, Some(blue)),
            IconStyle {
                glyph: "\u{f07b}",
                color: Some(blue)
            }
        );
    }

    #[test]
    fn known_dirs_keep_their_own_colors() {
        let ui = ui_icons(true);
        let blue = Color::Rgb(0x11, 0x22, 0x33);
        assert_eq!(
            icon_for(".git", true, false, &ui, Some(blue)),
            IconStyle {
                glyph: "\u{e5fb}",
                color: Some(Color::Rgb(0xf1, 0x4e, 0x32))
            }
        );
        assert_eq!(
            icon_for("superfile", true, false, &ui, Some(blue)),
            IconStyle {
                glyph: "\u{f069d}",
                color: Some(Color::Rgb(0xff, 0x6f, 0x00))
            }
        );
    }

    #[test]
    fn link_dir_uses_link_folder_icon() {
        let ui = ui_icons(true);
        assert_eq!(
            icon_for("anything", true, true, &ui, None),
            IconStyle {
                glyph: "\u{f482}",
                color: None
            }
        );
    }

    #[test]
    fn link_file_uses_link_file_icon() {
        let ui = ui_icons(true);
        assert_eq!(
            icon_for("test.js", false, true, &ui, None),
            IconStyle {
                glyph: "\u{f481}",
                color: None
            }
        );
    }

    #[test]
    fn file_miss_falls_back_to_file_glyph() {
        let ui = ui_icons(true);
        assert_eq!(
            icon_for("test.xyz", false, false, &ui, None),
            IconStyle {
                glyph: "\u{f15b}",
                color: None
            }
        );
    }

    #[test]
    fn file_extension_lookup() {
        let ui = ui_icons(true);
        assert_eq!(
            icon_for("test.js", false, false, &ui, None),
            IconStyle {
                glyph: "\u{e781}",
                color: Some(Color::Rgb(0xf3, 0x9c, 0x12))
            }
        );
        // Aliased extension: "gz" → "zip".
        assert_eq!(
            icon_for("archive.tar.gz", false, false, &ui, None),
            IconStyle {
                glyph: "\u{f410}",
                color: Some(Color::Rgb(0xe7, 0x4c, 0x3c))
            }
        );
        // Dotfile: filepath.Ext(".gitignore") is the whole name; alias
        // "gitignore" → "git".
        assert_eq!(
            icon_for(".gitignore", false, false, &ui, None),
            IconStyle {
                glyph: "\u{e702}",
                color: Some(Color::Rgb(0xe6, 0x7e, 0x22))
            }
        );
        // Extension lookup is case-insensitive.
        assert_eq!(
            icon_for("PHOTO.JPG", false, false, &ui, None),
            IconStyle {
                glyph: "\u{f1c5}",
                color: Some(Color::Rgb(0xe7, 0x4c, 0x3c))
            }
        );
    }

    #[test]
    fn full_name_takes_priority_over_extension() {
        let ui = ui_icons(true);
        // "package.json": full-name alias → "package" beats ext "json".
        assert_eq!(
            icon_for("package.json", false, false, &ui, None),
            IconStyle {
                glyph: "\u{f03d7}",
                color: Some(Color::Rgb(0x9b, 0x59, 0xb6))
            }
        );
        // Icons key hit directly on the full name.
        assert_eq!(
            icon_for("gulpfile.js", false, false, &ui, None),
            IconStyle {
                glyph: "\u{e610}",
                color: Some(Color::Rgb(0xe6, 0x7e, 0x22))
            }
        );
        // Dotfile full-name alias: ".env.local" → "env".
        assert_eq!(
            icon_for(".env.local", false, false, &ui, None),
            IconStyle {
                glyph: "\u{f462}",
                color: Some(Color::Rgb(0xee, 0xd6, 0x45))
            }
        );
        // "Makefile": no alias, but Icons has the lower-cased full name.
        assert_eq!(
            icon_for("Makefile", false, false, &ui, None),
            IconStyle {
                glyph: "\u{e20f}",
                color: Some(Color::Rgb(0x34, 0x98, 0xdb))
            }
        );
        // "LICENSE" via full-name alias "license" → "key".
        assert_eq!(
            icon_for("LICENSE", false, false, &ui, None),
            IconStyle {
                glyph: "\u{f43d}",
                color: Some(Color::Rgb(0xf1, 0xc4, 0x0f))
            }
        );
    }

    // -- ui_icons -----------------------------------------------------------

    #[test]
    fn ui_icons_nerdfont_values() {
        let ui = ui_icons(true);
        assert!(ui.nerdfont);
        assert_eq!(ui.space, " ");
        assert_eq!(ui.superfile, "\u{e6ad}");
        assert_eq!(ui.home, "\u{f02dc}");
        assert_eq!(ui.disk, "\u{f11f0}");
        assert_eq!(ui.music, "♬");
        assert_eq!(ui.checkbox_empty, "\u{f0131}");
        assert_eq!(ui.checkbox_checked, "\u{f0856}");
    }

    #[test]
    fn ui_icons_non_nerdfont_swaps() {
        let ui = ui_icons(false);
        assert!(!ui.nerdfont);
        // Swapped to empty / ASCII by InitIcon.
        assert_eq!(ui.space, "");
        assert_eq!(ui.superfile, "");
        assert_eq!(ui.home, "");
        assert_eq!(ui.download, "");
        assert_eq!(ui.documents, "");
        assert_eq!(ui.pictures, "");
        assert_eq!(ui.videos, "");
        assert_eq!(ui.music, "");
        assert_eq!(ui.templates, "");
        assert_eq!(ui.public_share, "");
        assert_eq!(ui.compress_file, "");
        assert_eq!(ui.extract_file, "");
        assert_eq!(ui.copy, "");
        assert_eq!(ui.cut, "");
        assert_eq!(ui.delete, "");
        assert_eq!(ui.cursor, ">");
        assert_eq!(ui.browser, "B");
        assert_eq!(ui.select, "S");
        assert_eq!(ui.error, "");
        assert_eq!(ui.warn, "");
        assert_eq!(ui.done, "");
        assert_eq!(ui.in_operation, "");
        assert_eq!(ui.directory, "");
        assert_eq!(ui.search, "");
        assert_eq!(ui.sort_asc, "^");
        assert_eq!(ui.sort_desc, "v");
        assert_eq!(ui.terminal, "");
        assert_eq!(ui.pinned, "");
        assert_eq!(ui.disk, "");
        // Kept (verified InitIcon quirk): desktop, trash, checkboxes.
        assert_eq!(ui.desktop, "\u{f01c4}");
        assert_eq!(ui.trash, "\u{f1f8}");
        assert_eq!(ui.checkbox_empty, "\u{f0131}");
        assert_eq!(ui.checkbox_checked, "\u{f0856}");
    }
}
