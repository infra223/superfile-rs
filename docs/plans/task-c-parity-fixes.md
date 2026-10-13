# TASK C — Parity fixes (trash init, copy_path select mode, border glyphs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the three highest-impact gaps found by the 2026-10-13 feature-parity audit vs Go superfile: initialize the XDG Trash layout at startup, make `copy_path` honor select mode, and wire the 10 `border_*` config glyphs into every renderer.

**Architecture:** Three independent fixes in existing modules. Trash: a startup `ensure_trash_layout` mirroring Go `trash.Init()` (`load_config.go:455`, `trash_linux.go:32`), with parameter-injected helpers so tests avoid env-var races. copy_path: extract Go's `copyPathText` (`handle_file_operations.go:570-583`) as a pure `App::copy_path_text`. Borders: add a `set: BorderSet<'_>` parameter to every component draw; `app.rs` passes `self.config.border_set()` (`config.rs:356`, currently dead code).

**Tech Stack:** Rust (edition 2021), `~/.cargo/bin/cargo`, existing deps only (no new crates).

**Spec:** the audit report in this session (2026-10-13); Go behavioral spec at `/home/flip/repos/superfile` (HEAD). Binding Go sources: `internal/trash/trash_linux.go`, `internal/common/load_config.go:455-462`, `internal/handle_file_operations.go:557-583`, `internal/common/style_function.go` + `ui/spf_renderers.go` (border glyphs).

## Global Constraints

- Go parity is the behavioral spec; user-visible strings must match Go exactly.
- Gates per task: `~/.cargo/bin/cargo check 2>&1 | grep "generated"` and `cargo check --tests` warning counts must not increase (baseline: 29 / 22); `cargo test` fully green with pristine output.
- TDD: every behavior change has a test watched failing first.
- Subagent reading discipline (local model): grep + ranged reads ≤150 lines; never read whole files >300 lines.
- Do not touch: `file_preview_width` validation, logging, Kitty protocol, README (out of scope for TASK C).
- Commit after each task with a message naming the Go source it mirrors.

## Review Focus

- Fresh system with no `~/.local/share/Trash` → delete-to-trash must work after startup (Task 1 test 1).
- Trash layout creation failure / no XDG data dir → startup must not panic; delete falls back to the existing permanent-delete confirmation (Task 1 test 2 pins `trash_root_in` → `None`).
- `copy_path` in Browser mode with stale selections → focused item only (Go `copyPathText`), Task 2 test 3.
- `copy_path` in Select mode with zero selected → focused item, Task 2 test 2.
- Default config → borders render byte-identically to today (no visual regression), Task 3 test 1.

---

### Task 1: Trash layout initialization

**Files:**
- Modify: `src/fileops.rs` (replace `trash_root()` at 427-438 with base-resolved helpers + `ensure_trash_layout`)
- Modify: `src/main.rs` (call `ensure_trash_layout` right after `paths.init()` succeeds, ~main.rs:306)
- Test: `src/fileops.rs` tests module

**Interfaces:**
- Consumes: existing `trash_root()` callers (`fileops.rs` trash_move, delete paths) — signature unchanged.
- Produces:
  - `pub fn trash_base() -> Option<PathBuf>` — `$XDG_DATA_HOME` if absolute, else `dirs::data_dir()` (same resolution as today's `trash_root`).
  - `pub fn trash_root_in(base: &Path) -> Option<PathBuf>` — `Some(base.join("Trash"))` iff it `is_dir()`.
  - `pub fn trash_root() -> Option<PathBuf>` — `trash_base().and_then(|b| trash_root_in(&b))`.
  - `pub fn ensure_trash_layout_in(base: &Path) -> std::io::Result<()>` — `create_dir_all` of `base/Trash/files` and `base/Trash/info` (Go `ensureTrashLayout`).
  - `pub fn ensure_trash_layout()` — `if let Some(b) = trash_base() { let _ = ensure_trash_layout_in(&b); }` — errors are logged with `eprintln!` only, never fatal (Go `InitTrash` logs and continues).

- [ ] **Step 1: Write failing tests** in `src/fileops.rs` tests module:

```rust
#[test]
fn test_ensure_trash_layout_creates_dirs() {
    let base = std::env::temp_dir().join(format!("spf_trash_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    ensure_trash_layout_in(&base).unwrap();
    assert!(base.join("Trash/files").is_dir());
    assert!(base.join("Trash/info").is_dir());
    assert_eq!(trash_root_in(&base), Some(base.join("Trash")));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn test_trash_root_in_missing_is_none() {
    let base = std::env::temp_dir().join(format!("spf_trash_missing_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    assert_eq!(trash_root_in(&base), None);
    let _ = std::fs::remove_dir_all(&base);
}
```

- [ ] **Step 2: Run, verify FAIL** — `~/.cargo/bin/cargo test test_ensure_trash_layout 2>&1 | tail -3` → compile error (functions not found). That is the RED.
- [ ] **Step 3: Implement** the four functions in `src/fileops.rs` per the Interfaces block; keep `trash_root()` semantics identical for existing callers.
- [ ] **Step 4: Wire startup** — in `src/main.rs`, immediately after the `paths.init()` error block: `fileops::ensure_trash_layout();` (non-fatal).
- [ ] **Step 5: Run tests, verify PASS** — `~/.cargo/bin/cargo test 2>&1 | tail -2` → 179 passed.
- [ ] **Step 6: Gates** — `cargo check` / `cargo check --tests` warning counts ≤ 29/22.
- [ ] **Step 7: Commit** `git add src/fileops.rs src/main.rs && git commit -m "fileops: initialize XDG Trash layout at startup (Go trash.Init parity)"`

---

### Task 2: copy_path honors select mode

**Files:**
- Modify: `src/app.rs` (`copy_path` at 1247-1253; add `copy_path_text`)
- Test: `src/app.rs` tests module (reuse the App-driven pattern from `test_update_metadata_dispatches_with_plugin_flag_off`)

**Interfaces:**
- Consumes: `Panel::mode` (pub field, `panel.rs:83`), `PanelMode::Select` (`panel.rs:30`), `Panel::selected_count()` (`panel.rs:542`), `Panel::selected_paths()` (`panel.rs:330`, already visible-order + cursor fallback), `Panel::cursor_entry()`.
- Produces: `fn copy_path_text(&self) -> String` — Go `copyPathText` semantics: empty focused panel → `""`; `mode == PanelMode::Select && selected_count() > 0` → `selected_paths()` joined with `"\n"`; otherwise focused item path or `""`. `copy_path` skips the clipboard call when the text is empty (Go `copyPath` early-return).

- [ ] **Step 1: Write failing tests** (three, App-driven; build App on a temp dir with 3 files, list via `relist_panels` + drain `rx` as in the metadata test):

```rust
#[test]
fn test_copy_path_text_select_mode_joins_selected() { /* Select mode, toggle 2 of 3
    entries via panel.toggle_selected (or the select hotkey), assert
    copy_path_text() == the two paths joined by "\n" in visible order. */ }

#[test]
fn test_copy_path_text_select_mode_empty_uses_cursor() { /* Select mode, no
    selection → focused path only. */ }

#[test]
fn test_copy_path_text_browser_mode_uses_cursor() { /* Browser mode with stale
    selected entries → focused path only (Go ignores selection outside Select mode). */ }
```

- [ ] **Step 2: Run, verify FAIL** — compile error (`copy_path_text` not found) = RED.
- [ ] **Step 3: Implement** `copy_path_text` in `src/app.rs` per Interfaces; rewrite `copy_path` to use it and return early on empty.
- [ ] **Step 4: Run tests, verify PASS** — full suite green (182 passed).
- [ ] **Step 5: Gates** — warning counts unchanged.
- [ ] **Step 6: Commit** `git commit -am "app: copy_path copies all selected paths in select mode (Go copyPathText parity)"`

---

### Task 3: Wire `border_*` config glyphs into all renderers

**Files:**
- Modify: `src/preview.rs:171-189`, `src/sidebar.rs:572-598`, `src/metadata.rs:173-214`, `src/clipboard.rs:86-104`, `src/processbar.rs:292-341`, `src/prompt.rs:207,427-453`, `src/zoxide.rs:333-356`, `src/dialogs.rs` (free helpers at 175/205/220/239 + `Notify::draw` 397, `SpfError::draw` 502, `SortMenu::draw` 612, `ThemeMenu::draw` 807, `HelpMenu::draw` 1233-1248, `TypingModal::draw` 1485), `src/panels.rs:236-299`, `src/app.rs` (all draw call sites), header comments `panels.rs:17`, `dialogs.rs:7`, `preview.rs` module doc.
- Test: `src/panels.rs` tests module + `src/config.rs` tests module.

**Interfaces:**
- Consumes: `Config::border_set(&self) -> BorderSet<'_>` (`config.rs:356`, already maps all 10 keys; `BorderSet` fields at `render.rs:29-40`).
- Produces: every component `draw` (and the `dialogs.rs` free draw helpers) gains a `set: BorderSet<'_>` parameter; every `BorderSet::plain()` production call site (grep `BorderSet::plain()` — the `render.rs:409` one is test-only and stays) uses `set` instead. `panels.rs:299` uses the `config` its draw already receives: `let set = config.border_set();`. `app.rs` obtains `let set = self.config.border_set();` once per render pass and passes it to all component draws.

- [ ] **Step 1: Write failing tests**:

```rust
#[test]
fn test_border_set_maps_custom_config() {
    // config.rs tests: Config with border_top "=", border_bottom_left "L", ...
    // assert border_set() fields equal the configured glyphs; with default
    // Config they equal the plain glyphs (no-regression line).
}

#[test]
fn test_draw_uses_custom_border_glyphs() {
    // panels.rs tests: Config with all border_* keys set to "="; build a
    // PanelGroup (existing test helpers), draw_panel into an RBuf; assert the
    // top border row contains '=' and no '─'.
}
```

- [ ] **Step 2: Run, verify FAIL** — `test_draw_uses_custom_border_glyphs` fails (draw hardcodes plain glyphs). If the signature change is needed first to compile, that compile failure is the RED for the threading.
- [ ] **Step 3: Thread `set`** — add the parameter to each draw/helper listed in Files, replace `BorderSet::plain()` with `set`; update `app.rs` call sites (pass `self.config.border_set()`; borrow is fine — draws only take `&self` of components). Update the three "house convention" header comments to say config glyphs are now honored.
- [ ] **Step 4: Run tests, verify PASS** — full suite green; existing render tests (which use plain glyphs) must keep passing unchanged.
- [ ] **Step 5: Smoke** — `cargo build --release`; run under the pty harness (`/tmp/opencode/ptyrun.py`) with a temp `--config-file` setting `border_top = "="` etc.; confirm the frame shows `=` borders and, with default config, unchanged `─` borders.
- [ ] **Step 6: Gates** — warning counts unchanged; `cargo test` green.
- [ ] **Step 7: Commit** `git commit -am "render: honor border_* config glyphs in all components (Go DefaultLipglossBorder parity)"`

---

## Out of scope (recorded for a future task)

Audit items 4–16: logging subsystem, config error/validation handling, `cd_on_quit` wrapper scripts, README flag claims, version-string alignment, Kitty protocol, first-run seeding details, `-c` alias, window title, file permission policy, external extract tools, minor divergences.
