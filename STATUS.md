# superfileR — port status

Rust port of Go `superfile` v1.6.0 → binary `spf`. Orchestrator + 1 subagent (zero-read prompts).

## Component status
| Module | Status |
|---|---|
| render, config, util, text_input, panel, fileops, preview, prompt, zoxide, main, icons, keys, fuzzy, event | ✅ compile-clean |
| clipboard.rs | ✅ done + verified |
| sidebar.rs | ✅ done + verified |
| processbar.rs | ✅ done + verified (553 lines, 6 tests) |
| metadata.rs | ✅ done + verified (1020 lines, 7 tests) |
| dialogs.rs | ✅ done + verified (2080 lines, 16 tests; notify/spferror/sort/theme/help/typing + first-use) |
| panels.rs | ✅ done + verified (1097 lines, 17 tests; PanelGroup + full panel render) |
| app.rs | ✅ TASK A done + verified (1746 lines; core state/event loop/layout/render/dispatch/preview/metadata/zoxide/prompt/quit/updates). TASK B (file ops) pending |
| **TESTS** | ✅ **135/135 passing** (first full run) |

## app.rs TASK B (next)
- paste/copy/cut/delete/permanent-delete/zip/extract + typing-modal confirm-create → worker spawns
  (fileops::spawn_worker already built; app has the TODO(task-B) markers + AsyncMsg handlers stubbed).
- spf_error skip/abort flow (SpfError::open + close→remaining; skip drops first remaining, empty→abort).
- processbar op wiring: add/update_progress/finish + has_running gating (quit-notify already wired).
- Go reference: `/home/flip/repos/superfile/src/internal/handle_file_operations.go` (598 lines).

## Bugs found & fixed in this session (first-ever test run)
1. **serde infinite recursion (CRITICAL, would crash app on ANY config load)**: container
   `#[serde(default)]` makes serde call `<Self as Default>::default()` inside `visit_map` for EVERY
   deserialization; `Default` impls deserialized TOML into the same struct → stack overflow.
   Fix: pure-literal `Config/Hotkeys/Theme::defaults()` + `#[serde(default = "Type::defaults")]`.
   Regression test: `config::tests::partial_config_fills_defaults`.
2. **keys.rs: `Key` PartialEq included `raw_char`** → "ctrl+c" etc. never matched real events.
   Manual PartialEq ignoring `raw_char`.
3. **preview.rs transpose/transverse**: loop bounds/indexes wrong for non-square images (panic).
   Fixed to new(x,y)=old(y,x) / old(w-1-y,h-1-x) with correct out-axes.
4. prompt.rs: `RunSpf` now carries the RESOLVED line (substitution runs exactly once, Go-faithful).
5. Five wrong TEST expectations fixed (icons go_ext/file_ext ×2, preview transverse tuple,
   sidebar slice len, text_input cursor-after-insert).

## Verification gate (per delivery)
- `~/.cargo/bin/cargo check 2>&1 | grep src/<file>.rs` → empty
- `~/.cargo/bin/cargo check --tests 2>&1 | grep src/<file>.rs` → empty
- `~/.cargo/bin/cargo test` → green
- Remaining 61 warnings = pre-existing dead-code for Task-B-consumed APIs (fileops ops, processbar
  add/finish, AsyncMsg op variants, Clipboard items, SpfError::open, …) — will drop as Task B lands.

## Notes
- Toolchain: `~/.cargo/bin/cargo`. Env quirks: std fork (no remove_all, no DirEntry::symlink_metadata), ratatui fork (local BorderSet, Cell API).
- serde_derive 1.0.229: container `#[serde(default)]` ⇒ `let __default = Default::default()` in visit_map (unconditional emission; only skipped by LLVM when provably unused). Never pair with a self-deserializing Default impl.
