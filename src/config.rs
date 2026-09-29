//! Configuration: XDG paths, app config, hotkeys, and themes.
//!
//! Defaults are embedded from `config/` (the same files shipped by the Go
//! version) and written to the XDG config dir on first run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

pub const APP_NAME: &str = "superfile";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const THEME_FILE_VERSION: &str = "v1";

pub const DEFAULT_CONFIG: &str = include_str!("../config/config.toml");
pub const DEFAULT_HOTKEYS: &str = include_str!("../config/hotkeys.toml");
pub const DEFAULT_VIM_HOTKEYS: &str = include_str!("../config/vimHotkeys.toml");

/// Built-in themes available for seeding the user theme directory.
pub const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("0x96f", include_str!("../config/theme/0x96f.toml")),
    ("ayu-dark", include_str!("../config/theme/ayu-dark.toml")),
    ("blood", include_str!("../config/theme/blood.toml")),
    ("catppuccin-frappe", include_str!("../config/theme/catppuccin-frappe.toml")),
    ("catppuccin-latte", include_str!("../config/theme/catppuccin-latte.toml")),
    ("catppuccin-macchiato", include_str!("../config/theme/catppuccin-macchiato.toml")),
    ("catppuccin-mocha", include_str!("../config/theme/catppuccin-mocha.toml")),
    ("dracula", include_str!("../config/theme/dracula.toml")),
    ("everforest-dark-hard", include_str!("../config/theme/everforest-dark-hard.toml")),
    ("everforest-dark-medium", include_str!("../config/theme/everforest-dark-medium.toml")),
    ("gruvbox", include_str!("../config/theme/gruvbox.toml")),
    ("gruvbox-dark-hard", include_str!("../config/theme/gruvbox-dark-hard.toml")),
    ("hacks", include_str!("../config/theme/hacks.toml")),
    ("kaolin", include_str!("../config/theme/kaolin.toml")),
    ("monokai", include_str!("../config/theme/monokai.toml")),
    ("nord", include_str!("../config/theme/nord.toml")),
    ("onedark", include_str!("../config/theme/onedark.toml")),
    ("poimandres", include_str!("../config/theme/poimandres.toml")),
    ("rose-pine", include_str!("../config/theme/rose-pine.toml")),
    ("sugarplum", include_str!("../config/theme/sugarplum.toml")),
    ("tokyonight", include_str!("../config/theme/tokyonight.toml")),
];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    /// Explicit overrides (CLI flags)
    pub config_file: Option<PathBuf>,
    pub hotkey_file: Option<PathBuf>,
}

impl Paths {
    pub fn new() -> Self {
        let base = || {
            dirs::config_dir()
                .or_else(|| Some(PathBuf::from(".").join(".config")))
                .unwrap_or_else(|| PathBuf::from("/etc/xdg"))
                .join(APP_NAME)
        };
        let state = || {
            dirs::state_dir()
                .or_else(dirs::data_dir)
                .unwrap_or_else(|| PathBuf::from("/var/lib"))
                .join(APP_NAME)
        };
        let data = || dirs::data_dir().unwrap_or_else(|| PathBuf::from("/var/lib")).join(APP_NAME);
        Self {
            config_dir: base(),
            state_dir: state(),
            data_dir: data(),
            config_file: None,
            hotkey_file: None,
        }
    }

    pub fn theme_dir(&self) -> PathBuf {
        self.config_dir.join("theme")
    }

    pub fn config_file_path(&self) -> PathBuf {
        self.config_file.clone().unwrap_or_else(|| self.config_dir.join("config.toml"))
    }

    pub fn hotkey_file_path(&self) -> PathBuf {
        self.hotkey_file
            .clone()
            .unwrap_or_else(|| self.config_dir.join("hotkeys.toml"))
    }

    pub fn last_dir_file(&self) -> PathBuf {
        self.state_dir.join("lastdir")
    }

    pub fn pinned_file(&self) -> PathBuf {
        self.data_dir.join("pinned.json")
    }

    pub fn toggle_dot_file(&self) -> PathBuf {
        self.data_dir.join("toggleDotFile")
    }

    pub fn toggle_footer_file(&self) -> PathBuf {
        self.data_dir.join("toggleFooter")
    }

    pub fn first_use_check_file(&self) -> PathBuf {
        self.data_dir.join("firstUseCheck")
    }

    pub fn last_check_version_file(&self) -> PathBuf {
        self.state_dir.join("lastCheckVersion")
    }

    pub fn theme_file_version_file(&self) -> PathBuf {
        self.data_dir.join("themeFileVersion")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("superfile.log")
    }

    /// Create XDG dirs and seed default files. Mirrors Go's `InitConfigFile`.
    pub fn init(&self) -> io::Result<()> {
        for d in [&self.config_dir, &self.state_dir, &self.data_dir, &self.theme_dir()] {
            fs::create_dir_all(d)?;
        }
        let cfg = self.config_file_path();
        if self.config_file.is_none() && !cfg.exists() {
            fs::write(&cfg, DEFAULT_CONFIG)?;
        }
        let hk = self.hotkey_file_path();
        if self.hotkey_file.is_none() && !hk.exists() {
            fs::write(&hk, DEFAULT_HOTKEYS)?;
        }
        // Seed themes (only missing ones)
        let marker = self.theme_file_version_file();
        let marker_ok = marker
            .to_str()
            .and_then(|s| fs::read_to_string(s).ok())
            .map(|v| v.trim() == THEME_FILE_VERSION)
            .unwrap_or(false);
        if !marker_ok {
            for (name, content) in BUILTIN_THEMES {
                let p = self.theme_dir().join(format!("{name}.toml"));
                if !p.exists() {
                    fs::write(&p, content)?;
                }
            }
            fs::write(&marker, THEME_FILE_VERSION)?;
        }
        Ok(())
    }

    /// Append missing keys to config/hotkeys files (`--fix-config-file` etc.).
    pub fn fix_files(&self) -> io::Result<()> {
        for (path, default) in [
            (self.config_file_path(), DEFAULT_CONFIG),
            (self.hotkey_file_path(), DEFAULT_HOTKEYS),
        ] {
            if !path.exists() {
                fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
                fs::write(&path, default)?;
                continue;
            }
            let existing = fs::read_to_string(&path)?;
            let mut missing = Vec::new();
            for line in default.lines() {
                let key = line.split(|c| c == '=' || c == '#').next().unwrap_or("").trim();
                if key.is_empty() || line.starts_with('#') {
                    continue;
                }
                let prefix = format!("{key} =");
                let alt = format!("{key}=");
                if !existing.contains(&prefix) && !existing.contains(&alt) {
                    missing.push(line.to_string());
                }
            }
            if !missing.is_empty() {
                let mut content = existing;
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str("\n# Appended by --fix-config-file / --fix-hotkeys\n");
                for line in missing {
                    content.push_str(&line);
                    content.push('\n');
                }
                fs::write(&path, content)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// App config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default = "Config::defaults")]
pub struct Config {
    pub theme: String,
    pub editor: String,
    pub dir_editor: String,
    #[serde(default)]
    pub open_with: HashMap<String, String>,
    pub auto_check_update: bool,
    pub cd_on_quit: bool,
    pub default_open_file_preview: bool,
    pub show_image_preview: bool,
    pub show_panel_footer_info: bool,
    pub default_directory: String,
    pub file_size_use_si: bool,
    pub default_sort_type: u8,
    pub sort_order_reversed: bool,
    pub case_sensitive_sort: bool,
    pub shell_close_on_success: bool,
    pub debug: bool,
    pub ignore_missing_fields: bool,
    pub page_scroll_size: usize,
    pub file_panel_extra_columns: usize,
    pub file_panel_name_percent: u8,
    pub nerdfont: bool,
    pub show_select_icons: bool,
    pub transparent_background: bool,
    pub file_preview_width: u8,
    pub enable_file_preview_border: bool,
    pub code_previewer: String,
    pub sidebar_width: u8,
    pub sidebar_sections: Vec<String>,
    pub border_top: String,
    pub border_bottom: String,
    pub border_left: String,
    pub border_right: String,
    pub border_top_left: String,
    pub border_top_right: String,
    pub border_bottom_left: String,
    pub border_bottom_right: String,
    pub border_middle_left: String,
    pub border_middle_right: String,
    pub metadata: bool,
    pub enable_md5_checksum: bool,
    pub zoxide_support: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Config {
    /// The default values (mirrors the embedded `config/config.toml`).
    ///
    /// CRITICAL: this must be a pure literal. Serde materializes the
    /// container default (`__default`) inside the generated `visit_map` for
    /// EVERY deserialization, so if `Default` deserialized TOML into `Config`
    /// the cycle `Default -> from_str -> visit_map -> Default` would recurse
    /// infinitely (stack overflow).
    pub(crate) fn defaults() -> Self {
        Self {
            theme: "catppuccin-mocha".into(),
            editor: String::new(),
            dir_editor: String::new(),
            open_with: HashMap::new(),
            auto_check_update: true,
            cd_on_quit: false,
            default_open_file_preview: true,
            show_image_preview: true,
            show_panel_footer_info: true,
            default_directory: ".".into(),
            file_size_use_si: false,
            default_sort_type: 0,
            sort_order_reversed: false,
            case_sensitive_sort: false,
            shell_close_on_success: false,
            debug: false,
            ignore_missing_fields: false,
            page_scroll_size: 0,
            file_panel_extra_columns: 0,
            file_panel_name_percent: 50,
            nerdfont: true,
            show_select_icons: true,
            transparent_background: false,
            file_preview_width: 0,
            enable_file_preview_border: false,
            code_previewer: String::new(),
            sidebar_width: 20,
            sidebar_sections: vec!["home".into(), "pinned".into(), "disks".into()],
            border_top: "─".into(),
            border_bottom: "─".into(),
            border_left: "│".into(),
            border_right: "│".into(),
            border_top_left: "╭".into(),
            border_top_right: "╮".into(),
            border_bottom_left: "╰".into(),
            border_bottom_right: "╯".into(),
            border_middle_left: "├".into(),
            border_middle_right: "┤".into(),
            metadata: false,
            enable_md5_checksum: false,
            zoxide_support: false,
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(cfg.validate())
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self).expect("config serializes");
        fs::write(path, text)
    }

    /// Clamp/validate values the same way the Go version does.
    fn validate(mut self) -> Self {
        if self.file_panel_name_percent < 25 || self.file_panel_name_percent > 100 {
            self.file_panel_name_percent = 50;
        }
        if self.default_sort_type > 4 {
            self.default_sort_type = 0;
        }
        for s in [
            &mut self.border_top,
            &mut self.border_bottom,
            &mut self.border_left,
            &mut self.border_right,
            &mut self.border_top_left,
            &mut self.border_top_right,
            &mut self.border_bottom_left,
            &mut self.border_bottom_right,
            &mut self.border_middle_left,
            &mut self.border_middle_right,
        ] {
            if s.chars().count() != 1 {
                *s = if s.is_empty() {
                    " ".to_string()
                } else {
                    s.chars().next().unwrap().to_string()
                };
            }
        }
        self
    }

    pub fn border_set(&self) -> crate::render::BorderSet<'_> {
        use crate::render::BorderSet;
        BorderSet {
            top: self.border_top.as_str(),
            bottom: self.border_bottom.as_str(),
            left: self.border_left.as_str(),
            right: self.border_right.as_str(),
            top_left: self.border_top_left.as_str(),
            top_right: self.border_top_right.as_str(),
            bottom_left: self.border_bottom_left.as_str(),
            bottom_right: self.border_bottom_right.as_str(),
            middle_left: self.border_middle_left.as_str(),
            middle_right: self.border_middle_right.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Hotkeys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default = "Hotkeys::defaults")]
pub struct Hotkeys {
    // global
    pub confirm: Vec<String>,
    pub cd_quit: Vec<String>,
    pub quit: Vec<String>,
    pub list_down: Vec<String>,
    pub list_up: Vec<String>,
    pub page_down: Vec<String>,
    pub page_up: Vec<String>,
    pub close_file_panel: Vec<String>,
    pub create_new_file_panel: Vec<String>,
    pub next_file_panel: Vec<String>,
    pub open_sort_options_menu: Vec<String>,
    pub pinned_directory: Vec<String>,
    pub previous_file_panel: Vec<String>,
    pub split_file_panel: Vec<String>,
    pub toggle_file_preview_panel: Vec<String>,
    pub toggle_reverse_sort: Vec<String>,
    pub focus_on_metadata: Vec<String>,
    pub focus_on_process_bar: Vec<String>,
    pub focus_on_sidebar: Vec<String>,
    pub file_panel_item_create: Vec<String>,
    pub file_panel_item_rename: Vec<String>,
    pub copy_items: Vec<String>,
    pub cut_items: Vec<String>,
    pub delete_items: Vec<String>,
    pub paste_items: Vec<String>,
    pub permanently_delete_items: Vec<String>,
    pub compress_file: Vec<String>,
    pub extract_file: Vec<String>,
    pub open_current_directory_with_editor: Vec<String>,
    pub open_file_with_editor: Vec<String>,
    pub change_panel_mode: Vec<String>,
    pub copy_path: Vec<String>,
    pub copy_present_working_directory: Vec<String>,
    pub open_command_line: Vec<String>,
    pub open_help_menu: Vec<String>,
    pub open_spf_prompt: Vec<String>,
    pub open_theme_menu: Vec<String>,
    pub open_zoxide: Vec<String>,
    pub toggle_dot_file: Vec<String>,
    pub toggle_footer: Vec<String>,
    // typing
    pub confirm_typing: Vec<String>,
    pub cancel_typing: Vec<String>,
    // normal mode
    pub parent_directory: Vec<String>,
    pub search_bar: Vec<String>,
    // select mode
    pub file_panel_select_mode_items_select_down: Vec<String>,
    pub file_panel_select_mode_items_select_up: Vec<String>,
    pub file_panel_select_all_items: Vec<String>,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Hotkeys {
    /// The default hotkey bindings (mirrors the embedded `config/hotkeys.toml`,
    /// including its empty-string placeholder entries).
    ///
    /// Must be a pure literal — see [`Config::defaults`] for why a
    /// TOML-parsing default would recurse infinitely through serde.
    pub(crate) fn defaults() -> Self {
        Self {
            confirm: vec!["enter".into(), "right".into(), "l".into()],
            cd_quit: vec!["Q".into(), "".into()],
            quit: vec!["q".into(), "esc".into()],
            list_down: vec!["down".into(), "j".into()],
            list_up: vec!["up".into(), "k".into()],
            page_down: vec!["pgdown".into(), "".into()],
            page_up: vec!["pgup".into(), "".into()],
            close_file_panel: vec!["w".into(), "".into()],
            create_new_file_panel: vec!["n".into(), "".into()],
            next_file_panel: vec!["tab".into(), "L".into()],
            open_sort_options_menu: vec!["o".into(), "".into()],
            pinned_directory: vec!["P".into(), "".into()],
            previous_file_panel: vec!["shift+left".into(), "H".into()],
            split_file_panel: vec!["N".into(), "".into()],
            toggle_file_preview_panel: vec!["f".into(), "".into()],
            toggle_reverse_sort: vec!["R".into(), "".into()],
            focus_on_metadata: vec!["m".into(), "".into()],
            focus_on_process_bar: vec!["p".into(), "".into()],
            focus_on_sidebar: vec!["s".into(), "".into()],
            file_panel_item_create: vec!["ctrl+n".into(), "".into()],
            file_panel_item_rename: vec!["ctrl+r".into(), "".into()],
            copy_items: vec!["ctrl+c".into(), "".into()],
            cut_items: vec!["ctrl+x".into(), "".into()],
            delete_items: vec!["ctrl+d".into(), "delete".into(), "".into()],
            paste_items: vec!["ctrl+v".into(), "ctrl+w".into(), "".into()],
            permanently_delete_items: vec!["D".into(), "".into()],
            compress_file: vec!["ctrl+a".into(), "".into()],
            extract_file: vec!["ctrl+e".into(), "".into()],
            open_current_directory_with_editor: vec!["E".into(), "".into()],
            open_file_with_editor: vec!["e".into(), "".into()],
            change_panel_mode: vec!["v".into(), "".into()],
            copy_path: vec!["ctrl+p".into(), "".into()],
            copy_present_working_directory: vec!["c".into(), "".into()],
            open_command_line: vec![":".into(), "".into()],
            open_help_menu: vec!["?".into(), "".into()],
            open_spf_prompt: vec![">".into(), "".into()],
            open_theme_menu: vec!["t".into(), "".into()],
            open_zoxide: vec!["z".into(), "".into()],
            toggle_dot_file: vec![".".into(), "".into()],
            toggle_footer: vec!["F".into(), "".into()],
            confirm_typing: vec!["enter".into(), "".into()],
            cancel_typing: vec!["ctrl+c".into(), "esc".into()],
            parent_directory: vec!["h".into(), "left".into(), "backspace".into()],
            search_bar: vec!["/".into(), "".into()],
            file_panel_select_mode_items_select_down: vec!["shift+down".into(), "J".into()],
            file_panel_select_mode_items_select_up: vec!["shift+up".into(), "K".into()],
            file_panel_select_all_items: vec!["A".into(), "".into()],
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Sanity check: global hotkeys should be unique across bindings.
    pub fn check_unique(&self) -> Vec<String> {
        let lists: &[(&str, &[String])] = &[
            ("confirm", &self.confirm),
            ("cd_quit", &self.cd_quit),
            ("quit", &self.quit),
            ("list_down", &self.list_down),
            ("list_up", &self.list_up),
            ("page_down", &self.page_down),
            ("page_up", &self.page_up),
            ("close_file_panel", &self.close_file_panel),
            ("create_new_file_panel", &self.create_new_file_panel),
            ("next_file_panel", &self.next_file_panel),
            ("open_sort_options_menu", &self.open_sort_options_menu),
            ("pinned_directory", &self.pinned_directory),
            ("previous_file_panel", &self.previous_file_panel),
            ("split_file_panel", &self.split_file_panel),
            ("toggle_file_preview_panel", &self.toggle_file_preview_panel),
            ("toggle_reverse_sort", &self.toggle_reverse_sort),
            ("focus_on_metadata", &self.focus_on_metadata),
            ("focus_on_process_bar", &self.focus_on_process_bar),
            ("focus_on_sidebar", &self.focus_on_sidebar),
            ("file_panel_item_create", &self.file_panel_item_create),
            ("file_panel_item_rename", &self.file_panel_item_rename),
            ("copy_items", &self.copy_items),
            ("cut_items", &self.cut_items),
            ("delete_items", &self.delete_items),
            ("paste_items", &self.paste_items),
            ("permanently_delete_items", &self.permanently_delete_items),
            ("compress_file", &self.compress_file),
            ("extract_file", &self.extract_file),
            ("open_current_directory_with_editor", &self.open_current_directory_with_editor),
            ("open_file_with_editor", &self.open_file_with_editor),
            ("change_panel_mode", &self.change_panel_mode),
            ("copy_path", &self.copy_path),
            ("copy_present_working_directory", &self.copy_present_working_directory),
            ("open_command_line", &self.open_command_line),
            ("open_help_menu", &self.open_help_menu),
            ("open_spf_prompt", &self.open_spf_prompt),
            ("open_theme_menu", &self.open_theme_menu),
            ("open_zoxide", &self.open_zoxide),
            ("toggle_dot_file", &self.toggle_dot_file),
            ("toggle_footer", &self.toggle_footer),
        ];
        let mut seen: HashMap<String, &str> = HashMap::new();
        let mut problems = Vec::new();
        for (name, keys) in lists {
            for k in *keys {
                if k.is_empty() {
                    continue;
                }
                if let Some(other) = seen.get(k) {
                    problems.push(format!("hotkey {k:?} bound to both {other} and {name}"));
                }
                seen.insert(k.clone(), name);
            }
        }
        problems
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default = "Theme::defaults")]
pub struct Theme {
    pub code_syntax_highlight: String,
    pub file_panel_border: String,
    pub sidebar_border: String,
    pub footer_border: String,
    pub file_panel_border_active: String,
    pub sidebar_border_active: String,
    pub footer_border_active: String,
    pub modal_border_active: String,
    pub full_screen_bg: String,
    pub file_panel_bg: String,
    pub sidebar_bg: String,
    pub footer_bg: String,
    pub modal_bg: String,
    pub full_screen_fg: String,
    pub file_panel_fg: String,
    pub sidebar_fg: String,
    pub footer_fg: String,
    pub modal_fg: String,
    pub cursor: String,
    pub correct: String,
    pub error: String,
    pub hint: String,
    pub cancel: String,
    pub gradient_color: Vec<String>,
    pub directory_icon_color: String,
    pub file_panel_top_directory_icon: String,
    pub file_panel_top_path: String,
    pub file_panel_item_selected_fg: String,
    pub file_panel_item_selected_bg: String,
    pub sidebar_title: String,
    pub sidebar_item_selected_fg: String,
    pub sidebar_item_selected_bg: String,
    pub sidebar_divider: String,
    pub modal_cancel_fg: String,
    pub modal_cancel_bg: String,
    pub modal_confirm_fg: String,
    pub modal_confirm_bg: String,
    pub help_menu_hotkey: String,
    pub help_menu_title: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Theme {
    /// The default theme values (mirrors the embedded
    /// `config/theme/catppuccin-mocha.toml`).
    ///
    /// Must be a pure literal — see [`Config::defaults`] for why a
    /// TOML-parsing default would recurse infinitely through serde.
    pub(crate) fn defaults() -> Self {
        Self {
            code_syntax_highlight: "catppuccin-mocha".into(),
            file_panel_border: "#6c7086".into(),
            sidebar_border: "#1e1e2e".into(),
            footer_border: "#6c7086".into(),
            file_panel_border_active: "#b4befe".into(),
            sidebar_border_active: "#f38ba8".into(),
            footer_border_active: "#a6e3a1".into(),
            modal_border_active: "#868686".into(),
            full_screen_bg: "#1e1e2e".into(),
            file_panel_bg: "#1e1e2e".into(),
            sidebar_bg: "#1e1e2e".into(),
            footer_bg: "#1e1e2e".into(),
            modal_bg: "#1e1e2e".into(),
            full_screen_fg: "#a6adc8".into(),
            file_panel_fg: "#a6adc8".into(),
            sidebar_fg: "#a6adc8".into(),
            footer_fg: "#a6adc8".into(),
            modal_fg: "#a6adc8".into(),
            cursor: "#f5e0dc".into(),
            correct: "#a6e3a1".into(),
            error: "#f38ba8".into(),
            hint: "#73c7ec".into(),
            cancel: "#eba0ac".into(),
            gradient_color: vec!["#89b4fa".into(), "#cba6f7".into()],
            directory_icon_color: String::new(),
            file_panel_top_directory_icon: "#a6e3a1".into(),
            file_panel_top_path: "#89b5fa".into(),
            file_panel_item_selected_fg: "#98D0FD".into(),
            file_panel_item_selected_bg: "#1e1e2e".into(),
            sidebar_title: "#74c7ec".into(),
            sidebar_item_selected_fg: "#A6DBF7".into(),
            sidebar_item_selected_bg: "#1e1e2e".into(),
            sidebar_divider: "#868686".into(),
            modal_cancel_fg: "#383838".into(),
            modal_cancel_bg: "#eba0ac".into(),
            modal_confirm_fg: "#383838".into(),
            modal_confirm_bg: "#89dceb".into(),
            help_menu_hotkey: "#89dceb".into(),
            help_menu_title: "#eba0ac".into(),
        }
    }

    pub fn load(dir: &Path, name: &str) -> io::Result<Self> {
        let text = fs::read_to_string(dir.join(format!("{name}.toml")))?;
        toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn list(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str().map(|s| s.to_string()) {
                    if let Some(stem) = n.strip_suffix(".toml") {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        names
    }

    fn parse_color(s: &str) -> Option<Color> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Color::Rgb(r, g, b))
    }

    /// Build a resolved palette. Empty theme fields fall back to the
    /// full-screen fg/bg (same inheritance behavior as the Go version).
    pub fn resolve(&self) -> Palette {
        let fs_fg = Self::parse_color(&self.full_screen_fg).unwrap_or(Color::White);
        let fs_bg = Self::parse_color(&self.full_screen_bg).unwrap_or(Color::Black);
        let fg = |s: &str| Self::parse_color(s).unwrap_or(fs_fg);
        let bg = |s: &str| Self::parse_color(s).unwrap_or(fs_bg);
        let col = |s: &str| Self::parse_color(s);
        Palette {
            full_screen_fg: fs_fg,
            full_screen_bg: fs_bg,
            file_panel_fg: fg(&self.file_panel_fg),
            file_panel_bg: bg(&self.file_panel_bg),
            file_panel_border: fg(&self.file_panel_border),
            file_panel_border_active: fg(&self.file_panel_border_active),
            file_panel_top_dir_icon: fg(&self.file_panel_top_directory_icon),
            file_panel_top_path: fg(&self.file_panel_top_path),
            file_panel_sel_fg: fg(&self.file_panel_item_selected_fg),
            file_panel_sel_bg: bg(&self.file_panel_item_selected_bg),
            sidebar_fg: fg(&self.sidebar_fg),
            sidebar_bg: bg(&self.sidebar_bg),
            sidebar_title: fg(&self.sidebar_title),
            sidebar_border: fg(&self.sidebar_border),
            sidebar_border_active: fg(&self.sidebar_border_active),
            sidebar_sel_fg: fg(&self.sidebar_item_selected_fg),
            sidebar_sel_bg: bg(&self.sidebar_item_selected_bg),
            sidebar_divider: fg(&self.sidebar_divider),
            footer_fg: fg(&self.footer_fg),
            footer_bg: bg(&self.footer_bg),
            footer_border: fg(&self.footer_border),
            footer_border_active: fg(&self.footer_border_active),
            modal_fg: fg(&self.modal_fg),
            modal_bg: bg(&self.modal_bg),
            modal_border: fg(&self.modal_border_active),
            modal_cancel_fg: fg(&self.modal_cancel_fg),
            modal_cancel_bg: bg(&self.modal_cancel_bg),
            modal_confirm_fg: fg(&self.modal_confirm_fg),
            modal_confirm_bg: bg(&self.modal_confirm_bg),
            cursor: fg(&self.cursor),
            correct: fg(&self.correct),
            error: fg(&self.error),
            hint: fg(&self.hint),
            cancel: fg(&self.cancel),
            directory_icon: col(&self.directory_icon_color),
            help_menu_hotkey: fg(&self.help_menu_hotkey),
            help_menu_title: fg(&self.help_menu_title),
            gradient: [
                Self::parse_color(self.gradient_color.first().map_or("", |v| v.as_str())),
                Self::parse_color(self.gradient_color.get(1).map_or("", |v| v.as_str())),
            ],
        }
    }
}

/// Resolved theme colors ready to use in rendering.
#[derive(Clone, Copy)]
pub struct Palette {
    pub full_screen_fg: Color,
    pub full_screen_bg: Color,
    pub file_panel_fg: Color,
    pub file_panel_bg: Color,
    pub file_panel_border: Color,
    pub file_panel_border_active: Color,
    pub file_panel_top_dir_icon: Color,
    pub file_panel_top_path: Color,
    pub file_panel_sel_fg: Color,
    pub file_panel_sel_bg: Color,
    pub sidebar_fg: Color,
    pub sidebar_bg: Color,
    pub sidebar_title: Color,
    pub sidebar_border: Color,
    pub sidebar_border_active: Color,
    pub sidebar_sel_fg: Color,
    pub sidebar_sel_bg: Color,
    pub sidebar_divider: Color,
    pub footer_fg: Color,
    pub footer_bg: Color,
    pub footer_border: Color,
    pub footer_border_active: Color,
    pub modal_fg: Color,
    pub modal_bg: Color,
    pub modal_border: Color,
    pub modal_cancel_fg: Color,
    pub modal_cancel_bg: Color,
    pub modal_confirm_fg: Color,
    pub modal_confirm_bg: Color,
    pub cursor: Color,
    pub correct: Color,
    pub error: Color,
    pub hint: Color,
    pub cancel: Color,
    pub directory_icon: Option<Color>,
    pub help_menu_hotkey: Color,
    pub help_menu_title: Color,
    pub gradient: [Option<Color>; 2],
}

impl Palette {
    pub fn gradient_color(&self, i: usize) -> Color {
        self.gradient
            .get(i)
            .copied()
            .flatten()
            .unwrap_or(self.file_panel_fg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg = Config::default();
        assert_eq!(cfg.theme, "catppuccin-mocha");
        assert_eq!(cfg.sidebar_width, 20);
        let hk = Hotkeys::default();
        assert!(hk.confirm.iter().any(|k| k == "enter"));
        assert!(hk.check_unique().is_empty());
    }

    // Regression: a PARTIAL config (missing most fields) must fill the gaps
    // from the defaults without recursing. Before the pure-literal defaults,
    // serde's container-default materialization re-entered Config::default()
    // -> from_str -> visit_map -> ... and overflowed the stack.
    #[test]
    fn partial_config_fills_defaults() {
        let cfg: Config = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(cfg.theme, "dracula");
        assert_eq!(cfg.nerdfont, true);
        assert_eq!(cfg.sidebar_width, 20);
        assert!(!cfg.cd_on_quit);
        let hk: Hotkeys = toml::from_str(r#"quit = ['ctrl+q']"#).unwrap();
        assert_eq!(hk.quit, vec!["ctrl+q"]);
        assert!(hk.confirm.iter().any(|k| k == "enter"));
        let th: Theme = toml::from_str(r##"full_screen_bg = "#000000""##).unwrap();
        assert_eq!(th.full_screen_bg, "#000000");
        assert_eq!(th.full_screen_fg, "#a6adc8");
    }

    #[test]
    fn theme_resolves() {
        let t = Theme::default();
        let p = t.resolve();
        assert_eq!(p.full_screen_bg, Color::Rgb(0x1e, 0x1e, 0x2e));
    }
}
