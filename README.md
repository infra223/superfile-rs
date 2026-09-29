# superfileR

A terminal file manager written in Rust. This is an **experimental**
reimplementation/translation of
[superfile](https://github.com/yorukot/superfile) v1.6.0 (written in Go),
developed with the help of local LLMs — specifically
Qwen3.8 27B (Q5_K_XL quantization, running locally via llama.cpp) —
driven by the [OpenCode](https://opencode.ai) coding agent.

The project's intention is to keep full feature parity with the original
Go-based project.

The binary is `spf`.

## Features

- Multi-panel file browser (up to 10 panels) with sidebar, breadcrumbs, and zoxide jump
- File preview: images (ANSI half-blocks), PDF, video, music, and plain text
- Clipboard footer: copy / cut / paste with progress, and skip / abort on errors
- XDG trash (delete), permanent delete, rename, create, zip, and extract
- Metadata panel (file info, image/video/music metadata via exiftool & ffprobe)
- SPF prompt: `:` shell / `spf` commands, `>` shell with output capture
- 21 built-in TOML themes (Catppuccin, Dracula, Solarized, Gruvbox, Tokyo Night, ...)
- User config: `hotkeys.toml`, `config.toml`, custom themes
- Mouse wheel scrolling, fuzzy search (`/`), sort menu, theme menu, help modal
- `cd_on_quit` and last-directory restoration, automatic update check

## Building

Requires a Rust toolchain (1.98+ recommended) and a real TTY to run.

```sh
cargo build --release
./target/release/spf [path]
```

Debug build: `cargo build` → `./target/debug/spf`

Run the test suite: `cargo test`

## CLI flags

| Flag | Description |
|---|---|
| `--theme, -T <theme>` | Load a theme by name or path |
| `--vim` | Use vim-style hotkeys |
| `--cd-on-quit` | Print the final directory on exit (for `cd $(spf --cd-on-quit)`) |
| `--chooser-file <path>` | Write the selected file to `path` on open, then exit |

## Configuration

On first run, superfileR seeds its data directory (XDG-compliant) with:

- `config.toml` — behavior settings (preview, trash, update check, borders, ...)
- `hotkeys.toml` — all key bindings (plus `vimHotkeys.toml` for `--vim`)
- `theme/` — the 21 built-in themes

Edit those files to customize.

## Optional tools

SuperfileR shells out to external tools when available and degrades gracefully
without them:

- `zoxide` — directory jumping
- `exiftool` — image/audio/video metadata
- `ffmpeg` / `ffprobe` — video & music preview
- `pdftoppm` / `gs` — PDF preview
- `xdg-open` — opening the editor / file

## License

MIT — original project by [Yorukot](https://github.com/yorukot/superfile).
