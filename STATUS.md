# superfileR — port status

Rust port of Go `superfile` v1.6.0 → binary `spf`. Orchestrator + 1 subagent (zero-read prompts).

## Component status
| Module | Status |
|---|---|
| render, config, util, text_input, panel, preview, prompt, zoxide, main, icons, keys, fuzzy, event | ✅ compile-clean |
| clipboard.rs | ✅ done + verified |
| sidebar.rs | ✅ done + verified |
| processbar.rs | ✅ done + verified (555 lines, 6 tests) |
| metadata.rs | ✅ done + verified (1020 lines, 7 tests) |
| dialogs.rs | ✅ done + verified (2076 lines, 18 tests; notify/spferror/sort/theme/help/typing + first-use) |
| panels.rs | ✅ done + verified (1097 lines, 17 tests; PanelGroup + full panel render) |
| fileops.rs | ✅ TASK B done + verified (1688 lines, 28 tests; paste/compress/extract/delete/create workers, Go naming + totals) |
| app.rs | ✅ TASK A + TASK B done + verified (2408 lines, 11 tests; core state/event loop/layout/render/dispatch/preview/metadata/zoxide/prompt/quit/updates + all file-op triggers + spf-error skip/abort) |
| **TESTS** | ✅ **170/170 passing** |

## TASK B complete (file ops)
- Triggers wired: copy/cut/paste (`p`/`P`), delete trash+permanent (`d`/`D`), create file/dir (`n`/`N`),
  compress (`ctrl+a`), extract (`ctrl+e`) → `App::spawn_op` worker + processbar lifecycle
  (add/update_progress/finish) + `OpRecord` registry keyed by io_req id.
- spf-error skip/abort (Go `spfErrorModelOpenKey` + `FileListErrorState`): Skip resumes with the
  remaining items under the SAME op id, original total, `done_offset = total - remaining.len()` (R5);
  empty remaining aborts. Abort sets the cancel flag, finishes the process Failed with the OpFail
  message, drops the record (R4). Full skip-resume success finishes Successful (R3).
- Compress: Go `getZipArchiveName` naming (R10: `.gitignore` → `.gitignore.zip`), Go
  `validateCompressOperation` wording ("Invalid zip target name" / "Invalid file/dir to compress"),
  duplicate target → renamed via `unique_target` to `name(N).zip` (revised R6, Go
  `renameIfDuplicate` parity); Cancelled "File already exists" only when the counter exhausts;
  explicit target path to the worker (no internal rename).
- Extract: gate = last extension in {zip, bz, gz, iso, rar, 7z, tar} lowercase (R7 — `.xz`/`.tgz`/
  `.bz2` NOT extractable), output dir = strip ALL trailing dot-suffixes keeping leading dot (R8) +
  `unique_target`, created before the worker (Go `MkdirAll`).
- Quit gating verified: `QuitState::Initiated` + `processbar.has_running()` → ConfirmInitiated +
  "Confirm to quit superfile" notify (Go progression, app.rs handle_key). No fix needed.
- `unique_target` exhaustion (10k collisions) returns the input path; `create_item` treats it as an
  error (Go parity). Copy/move/trash call sites would overwrite the existing target in that
  pathological case — noted, non-issue in practice (Go errors instead).

## Go parity smoke (Task 3, scripted pty: `script` + stty 40x120, keys piped)
Ran `target/debug/spf` vs `/usr/bin/spf` (Go v1.6.0) in scratch dirs:
- Compress fresh file: both create `notes.zip`, bar shows "Compressing notes.txt" → "Compressed notes.zip". ✅
- Compress duplicate target: Go v1.6.0 `getZipArchiveName` calls `renameIfDuplicate` and creates
  `notes(1).zip` (the zipSources "File already exists" Cancelled block only fires on a race).
  **Fixed per revised R6 ruling**: Rust now renames via `unique_target` (`a.zip` → `a(1).zip`);
  Cancelled "File already exists" remains only for counter exhaustion.
- Extract `archive.tar`: both create dir `archive/`, bar shows "Extracting/Extracted archive.tar". ✅
- Smoke found + fixed: `relayout` panicked (subtract overflow) on a 0x0 pty — now `saturating_sub`.

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
6. (Task 3) app.rs `relayout` subtract-overflow panic on 0x0 terminal (found by pty smoke).

## Verification gate (per delivery)
- `~/.cargo/bin/cargo check 2>&1 | grep src/<file>.rs` → empty (app/fileops/processbar clean)
- `~/.cargo/bin/cargo check --tests 2>&1 | grep src/<file>.rs` → empty
- `~/.cargo/bin/cargo test` → 170/170 green
- Remaining 30 warnings = pre-existing dead-code for other features (Metadata/Md5/SubstDone/
  UpdateCheck variants, req_id fields, unused util/sidebar helpers). All Task-B APIs are consumed;
  `TODO(task-B)` markers: 0.

## Notes
- Toolchain: `~/.cargo/bin/cargo`. Env quirks: std fork (no remove_all, no DirEntry::symlink_metadata), ratatui fork (local BorderSet, Cell API).
- serde_derive 1.0.229: container `#[serde(default)]` ⇒ `let __default = Default::default()` in visit_map (unconditional emission; only skipped by LLVM when provably unused). Never pair with a self-deserializing Default impl.
- Worker progress stream (Go-faithful): Copy/Cut cross-device emit per-file callback Progress + per-item update Progress + final done update (single-file copy = two same-`done` updates); Delete/Create/Extract one update per item.
