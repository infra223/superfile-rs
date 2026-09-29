//! superfile (Rust) — `spf` entry point.
//!
//! Handles the CLI surface (flags, `path-list` subcommand, `--debug-info`)
//! and then hands off to the TUI in [`app::App`].

mod app;
mod clipboard;
mod config;
mod dialogs;
mod event;
mod fileops;
mod fuzzy;
mod icons;
mod keys;
mod metadata;
mod panel;
mod panels;
mod preview;
mod processbar;
mod prompt;
mod render;
mod sidebar;
mod text_input;
mod util;
mod zoxide;

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "spf",
    version = env!("CARGO_PKG_VERSION"),
    about = "Pretty fancy and modern terminal file manager"
)]
struct Cli {
    /// Print debug information
    #[arg(long = "debug-info")]
    debug_info: bool,

    /// Add any missing hotkeys to the hotkey config file
    #[arg(long = "fix-hotkeys")]
    fix_hotkeys: bool,

    /// Add any missing fields to the config file
    #[arg(long = "fix-config-file")]
    fix_config_file: bool,

    /// Print the last dir to stdout on exit (to use for cd)
    #[arg(long = "print-last-dir")]
    print_last_dir: bool,

    /// Specify the path to a different config file
    #[arg(long = "config-file")]
    config_file: Option<PathBuf>,

    /// Specify the path to a different hotkey file
    #[arg(long = "hotkey-file")]
    hotkey_file: Option<PathBuf>,

    /// On trying to open any file, superfile writes its path to this file, and exits
    #[arg(long = "chooser-file")]
    chooser_file: Option<PathBuf>,

    /// Paths to open in the first file panel
    #[arg(trailing_var_arg = true)]
    paths: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the path to the configuration and directory
    #[command(alias = "pl")]
    PathList {
        /// Print path to lastdir file (where last dir is written when cd_on_quit is true)
        #[arg(long = "lastdir-file")]
        lastdir_file: bool,
    },
}

/// urfave/cli permits multi-character short aliases (`-di`, `-fh`, `-fch`,
/// `-pld`, `-hf`, `-cf`, `-ld`) which clap does not. Normalise them to their
/// long forms before parsing.
fn normalize_args(args: Vec<String>) -> Vec<String> {
    const MAP: &[(&str, &str)] = &[
        ("-di", "--debug-info"),
        ("-fh", "--fix-hotkeys"),
        ("-fch", "--fix-config-file"),
        ("-pld", "--print-last-dir"),
        ("-hf", "--hotkey-file"),
        ("-cf", "--chooser-file"),
        ("-ld", "--lastdir-file"),
    ];
    args.into_iter()
        .map(|a| MAP.iter().find(|(k, _)| *k == a).map(|(_, v)| v.to_string()).unwrap_or(a))
        .collect()
}

// ---------------------------------------------------------------------------
// debug-info
// ---------------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const UNDERLINE: &str = "\x1b[4m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";

fn title(s: &str) {
    println!("\n{UNDERLINE}{GREEN}{BOLD}{s}{RESET}");
}

fn kv(key: &str, value: &str) {
    // Absolute-path values that don't exist get a "(Not Found)" warning.
    let value = if Path::new(value).is_absolute() && !Path::new(value).exists() {
        format!("{value} {RED}(Not Found){RESET}")
    } else {
        value.to_string()
    };
    print!("{CYAN}{BOLD}{:<20}{RESET}: {}\n", key, value);
}

fn kv_env(key: &str) {
    let val = std::env::var(key).unwrap_or_default();
    kv(key, if val.is_empty() { "Not Set" } else { &val });
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn dep(name: &str, flag: &str) {
    match find_in_path(name) {
        None => print!("{CYAN}{BOLD}{:<20}{RESET}: {RED}Not Found{RESET}\n", name),
        Some(path) => {
            let mut status = format!("Found at {}", path.display());
            if !flag.is_empty() {
                if let Ok(out) = Command::new(name).args(flag.split_whitespace()).output() {
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        let first = text.lines().next().unwrap_or("").trim().to_string();
                        if !first.is_empty() {
                            status = if first.chars().count() > 50 {
                                format!("{}...", first.chars().take(50).collect::<String>())
                            } else {
                                first
                            };
                        }
                    }
                }
            }
            print!("{CYAN}{BOLD}{:<20}{RESET}: {GREEN}{status}{RESET}\n", name);
        }
    }
}

fn print_debug_info(paths: &config::Paths) {
    println!();
    title("Superfile");
    kv("Version", config::VERSION);

    title("System");
    kv("OS", "linux");
    let arch = std::env::consts::ARCH;
    kv("Arch", arch);
    if let Ok(out) = Command::new("uname").arg("-r").output() {
        if out.status.success() {
            let k = String::from_utf8_lossy(&out.stdout).trim().to_string();
            kv("Kernel", &k);
        }
    }

    title("Configuration");
    kv("Config File", &paths.config_file_path().display().to_string());
    kv("Hotkeys File", &paths.hotkey_file_path().display().to_string());
    kv("Theme Folder", &paths.theme_dir().display().to_string());
    kv("Log File", &paths.log_file().display().to_string());
    kv("Data Dir", &paths.data_dir.display().to_string());

    title("Environment");
    kv_env("TERM");
    kv_env("TERM_PROGRAM");
    kv_env("TERM_PROGRAM_VERSION");
    kv_env("SHELL");
    kv_env("EDITOR");
    kv_env("VISUAL");
    kv_env("XDG_SESSION_TYPE");
    kv_env("WAYLAND_DISPLAY");
    kv_env("DISPLAY");

    title("Dependencies");
    dep("ffmpeg", "-version");
    dep("pdftoppm", "-v");
    dep("exiftool", "-ver");
    dep("bat", "--version");
    dep("zoxide", "--version");
    dep("xdg-open", "--version");
    dep("wl-copy", "--version");
    dep("xclip", "-version");
    dep("xsel", "--version");
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

fn path_list(paths: &config::Paths, lastdir_file: bool) {
    if lastdir_file {
        println!("{}", paths.last_dir_file().display());
        return;
    }
    let pad = |label: &str, color: &str| format!("{:<55}", format!("{color}{label}{RESET}"));
    println!(
        "{} {}",
        pad("[Configuration file path]", CYAN),
        paths.config_file_path().display()
    );
    println!(
        "{} {}",
        pad("[Hotkeys file path]", CYAN),
        paths.hotkey_file_path().display()
    );
    println!(
        "{} {}",
        pad("[Log file path]", GREEN),
        paths.log_file().display()
    );
    println!(
        "{} {}",
        pad("[Configuration directory path]", RED),
        paths.config_dir.display()
    );
    println!(
        "{} {}",
        pad("[Data directory path]", RED),
        paths.data_dir.display()
    );
}

/// Returns true if this is the first launch (intro modal should be shown).
fn check_first_use(paths: &config::Paths) -> bool {
    let f = paths.first_use_check_file();
    if !f.exists() {
        let _ = std::fs::File::create(&f);
        true
    } else {
        false
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cli = Cli::parse_from(normalize_args(args));

    let mut paths = config::Paths::new();
    if let Some(cf) = cli.config_file.clone() {
        if !cf.exists() {
            eprintln!(
                "Error: While reading config file '{}' from argument : does not exist",
                cf.display()
            );
            std::process::exit(1);
        }
        paths.config_file = Some(cf);
    }
    if let Some(hf) = cli.hotkey_file.clone() {
        if !hf.exists() {
            eprintln!(
                "Error: While reading hotkey file '{}' from argument : does not exist",
                hf.display()
            );
            std::process::exit(1);
        }
        paths.hotkey_file = Some(hf);
    }

    if let Some(Cmd::PathList { lastdir_file }) = &cli.command {
        path_list(&paths, *lastdir_file);
        return;
    }

    if cli.debug_info {
        print_debug_info(&paths);
        return;
    }

    // Create XDG dirs + seed default files.
    if let Err(e) = paths.init() {
        eprintln!("Error creating directories: {e}");
        std::process::exit(1);
    }

    if cli.fix_config_file || cli.fix_hotkeys {
        if let Err(e) = paths.fix_files() {
            eprintln!("Error fixing config files: {e}");
            std::process::exit(1);
        }
    }

    let first_use = check_first_use(&paths);
    let first_panel_paths: Vec<PathBuf> = if cli.paths.is_empty() {
        vec![PathBuf::from("")]
    } else {
        cli.paths.clone()
    };

    let mut app = app::App::new(paths, first_panel_paths, first_use, cli.chooser_file);
    match app.run() {
        Ok(last_dir) => {
            app.check_for_updates();
            if cli.print_last_dir {
                println!("{last_dir}");
            }
        }
        Err(e) => {
            eprintln!("Alas, there's been an error: {e}");
            std::process::exit(1);
        }
    }
}
