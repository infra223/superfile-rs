# Plan: TASK B — file-operations wiring in `app.rs` (superfileR)

Rust port of Go `superfile` v1.6.0 → binary `spf`. TASK A (app core: state,
event loop, layout, render, dispatch, preview, metadata, zoxide, prompt, quit)
is done and verified; 135 tests pass. TASK B wires the file operations into
the app: copy/cut/paste, delete (trash + permanent), create, compress,
extract, the spf-error skip/abort flow, and process-bar bookkeeping.

## Spec authority

The Go project at `/home/flip/repos/superfile/src/internal` (v1.6.0) is the
binding behavioral spec. Primary references:

- `handle_file_operations.go` — delete/copy/cut/paste triggers, validation, processors
- `handle_modal.go` — `createItem`, `getCreateCmd`, `createOperation`, `confirmRename`
- `file_operations.go` — `moveElement`, `pasteDir`, `actualPasteOperation`, `isAncestor`
- `file_operations_compress.go` — `getCompressSelectedFilesCmd`, `zipSources`, `getZipArchiveName`
- `file_operations_extract.go` — `extractCompressFile`
- `function.go` — `renameIfDuplicate`, `countFiles`, `countReadableFiles`, `isExternalDiskPath`, `checkFileNameValidity`
- `model_msg.go` — post-op effects (`PasteOperationMsg`, `DeleteOperationMsg` ApplyToModel)
- `key_function.go` + `ui/spferror/model.go` — skip/abort flow
- `common/predefined_variable.go` — exact modal titles/contents
- `common/string_function.go` — `IsExtensionExtractable`, `FileNameWithoutExtension`

## Global constraints

- Behavior, user-visible strings, and process-bar semantics must match Go.
  Implementation must be idiomatic Rust (ownership, `Result`, no `unwrap()` on
  user/IO paths, no Go idioms, minimal clones).
- Toolchain: `~/.cargo/bin/cargo`. Verification gate per delivery:
  1. `cargo check 2>&1 | grep src/<changed>.rs` → empty
  2. `cargo check --tests 2>&1 | grep src/<changed>.rs` → empty
  3. `cargo test` → fully green (135+ tests)
- Env quirks: std fork has no `Path::remove_all` / `DirEntry::symlink_metadata`
  (use `std::fs::symlink_metadata`); ratatui fork has local `BorderSet`/`Cell` API.
- Components `dialogs.rs`, `processbar.rs`, `clipboard.rs`, `panel.rs`,
  `panels.rs`, `sidebar.rs` are done+verified: wire them from `app.rs`; touch
  them only where a task explicitly allows.
- Worker protocol already built in `fileops.rs`: `spawn_worker` /
  `spawn_remaining` / `OpKind` / `WorkerMsg`. `App` bridges `WorkerMsg` →
  `AsyncMsg` (`OpProgress` / `OpFail` / `OpDone { op, cancelled }`).
- Existing APIs to use: `processbar::{add, update_progress, finish, has_running,
  Operation, State}`, `clipboard::{reset, add, set_items, is_cut, items,
  prune_inaccessible_and_get}`, `dialogs::{SpfError::open/close, Notify::open,
  ConfirmAction}`, `panel::{selected_paths, selected_count, reset_selected,
  empty, dirty}`.
- Exact Go strings (verbatim):
  - `TrashWarnTitle = "Are you sure you want to move this to trash can"`,
    `TrashWarnContent = "This operation will move file or directory to trash can."`
  - `PermanentDeleteWarnTitle = "Are you sure you want to completely delete"`,
    `PermanentDeleteWarnContent = "This operation cannot be undone and your data will be completely lost."`
  - Paste validation errors: `"cannot paste into parent directory of source, srcPath : {src}, panelLocation : {dest}"`,
    `"cannot paste a directory into itself"`,
    `"cannot copy and paste a directory into itself or its subdirectory"` /
    `"cannot cut and paste a directory into itself or its subdirectory"`
  - Notify titles: `"Invalid paste location"`, `"Invalid zip target name"`,
    `"Invalid file/dir to compress"` (all with `ConfirmAction::NoAction`)
  - Compress duplicate: process `Cancelled` with error `"File already exists"`
- All `TODO(task-B)` markers in `app.rs` must be gone by the end of Task 3,
  and the pre-existing dead-code warnings for Task-B APIs must drop to zero
  (or near-zero) warnings.

## Rulings (controller decisions, binding for all tasks)

- **R1 — unique_target naming**: Go `renameIfDuplicate` produces `name(N).ext`
  (resuming from an existing `(N)` suffix). Rust `fileops::unique_target`
  currently produces `name.N.ext`. Align Rust to Go's `name(N).ext` scheme
  (cap 10_000; on exhaustion return the original path — the subsequent fs call
  will then fail, mirroring Go's error path). Update affected tests.
- **R2 — paste totals/progress**: Go's paste process total is the recursive
  file count (`countFiles`), with per-file progress (`CurrentFile = base(path)`,
  `Done++` per copied file) for copy and for cross-device cut; same-device cut
  uses a fast whole-tree rename with no per-file progress and `Done = Total` at
  the end. Implement the same in `fileops.rs` (add `count_files`, a progress
  callback on copy, `is_same_partition` helper).
- **R3 — skip-retry success state**: Go's captured `Process.State` stays
  `Failed` across a skip-retry, so a fully successful skip-retry never marks
  the process `Successful` (internally inconsistent Go quirk). Rust follows
  the clear intent: a skip-retry that completes without failure finishes the
  process `Successful`.
- **R4 — processbar on OpFail**: Go does NOT finalize the process-bar entry on
  failure (it stays in-operation until abort → `Failed`, or skip-retry
  completion). Rust: `OpFail` keeps the entry in-operation, opens the
  spf-error modal, and keeps the op record; `Abort` finishes it `Failed` with
  the error message.
- **R5 — skip remaining semantics**: Go's `fileList` includes the failed item
  and `Skip` drops the first element (`len <= 1` → abort). Rust's
  `WorkerMsg::Fail.remaining` already excludes the failed item: `remaining`
  empty → abort; else `spawn_remaining` with the same op id and a
  `done_offset = items_total - remaining.len()` so progress continues
  absolutely (extend `spawn_remaining` with `done_offset`).
- **R6 — compress duplicate target**: Go cancels with `"File already exists"`
  (no auto-rename). Rust must match: app checks the target before spawning;
  on duplicate, add a process-bar entry and immediately `finish(Cancelled,
  Some("File already exists"))`. `compress_items` takes an explicit target
  path (no `unique_target` inside).
- **R7 — extractable extensions gate**: Go's effective gate (last extension,
  lowercased) is `{zip, bz, gz, iso, rar, 7z, tar}` (its `.tar.gz` map entry
  is unreachable; `.bz2/.xz/.tgz/...` are NOT extractable in Go). Align
  `fileops::is_extractable` to exactly this set. Rust's internal extract
  support may stay broader, but the trigger gate matches Go.
- **R8 — extract output dir**: Go `FileNameWithoutExtension` strips ALL
  trailing dot-suffixes (loop), keeping a leading dot (`".gitignore"` stays).
  Rust already strips compound suffixes; align to Go's loop semantics exactly.
- **R9 — panel refresh after ops**: Go panels refresh via directory watchers;
  the Rust port uses `dirty` + relist. On op completion (`OpDone`) mark all
  panels dirty so the next `update_panels` relist refreshes them.
- **R10 — Go's cwd-relative `renameIfDuplicate(zipName)` in
  `getCompressSelectedFilesCmd`** is a Go bug (checks duplicates against cwd,
  then joins with panel location). Rust computes the zip name from the first
  source's base name only; duplicate detection happens against the real
  target in the panel location (per R6).

## Task 1: Op infrastructure + copy/cut/paste

Files: `src/app.rs`, `src/fileops.rs` (+ tests).

1. `App` gains an op registry: `ops: HashMap<u64, OpRecord>` with
   `OpRecord { kind: OpKind, dest: PathBuf, cancel: Arc<AtomicBool>, total: usize }`.
   Op ids come from the existing `io_req` counter.
2. `App::spawn_op(&mut self, kind: OpKind, items: Vec<PathBuf>, dest: PathBuf)`:
   - allocate `op_id`, `cancel = Arc::new(AtomicBool::new(false))`, a
     `mpsc::channel::<WorkerMsg>`; call `fileops::spawn_worker`; spawn a
     forwarder thread that maps every `WorkerMsg` to the matching `AsyncMsg`
     (`Progress→OpProgress`, `Fail→OpFail`, `Done→OpDone{cancelled: cancel flag}`)
     and sends it to `self.tx`;
   - `processbar.add(op_id, operation, base(items[0]), total)` where total is
     per R2 for Copy/Cut (recursive file count via new `fileops::count_files`)
     and `items.len()` for Delete/Create;
   - insert the `OpRecord`.
3. `fileops.rs` changes (per R1, R2):
   - `unique_target` → Go `renameIfDuplicate` semantics (`name(N).ext`, resume
     counter from existing `(N)`, cap 10_000); update its tests and any test
     asserting the old `.N` format.
   - `count_files(path) -> usize` (recursive count of non-directory entries,
     Go `countFiles`).
   - `is_same_partition(src, dst) -> bool` via `std::os::unix::fs::MetadataExt::dev()`
     on canonicalized paths.
   - copy/cut progress: `copy_item`/`copy_dir_recursive`/`move_item` accept an
     optional progress callback `&mut dyn FnMut(&Path)` invoked once per copied
     file; `spawn_worker` wires it to `WorkerMsg::Progress { file: base(path),
     done, total }` with absolute `done`/`total` per R2. Same-partition cut
     keeps the fast whole-tree rename (no per-file progress; final progress
     `done = total`).
4. Copy/cut triggers (Go `copySingleItem` / `copyMultipleItem`):
   - Normal (browser) mode: `clipboard_ui.reset(cut)`; if the focused panel is
     non-empty, `add(focused item location)`.
   - Select mode: `clipboard_ui.reset(cut)`; if `selected_count() > 0`,
     `set_items(selected_paths())` (verify `selected_paths` returns
     visible-order like Go's `GetSelectedLocationsSortedAsVisible`; fix panel
     only if it does not).
5. Paste trigger (Go `getPasteItemCmd` + `validatePasteOperation`):
   - `items = clipboard_ui.prune_inaccessible_and_get()`; empty → return.
   - `dest = focused panel location`; validate (in this order, per item):
     a. `cut && parent(src) == dest` → error string from Global constraints;
     b. `cut && src == dest` → `"cannot paste a directory into itself"`;
     c. `fileops::is_ancestor(src, dest)` → `"cannot {copy|cut} and paste a
        directory into itself or its subdirectory"`.
   - On validation error: `notify.open("Invalid paste location", err, NoAction)`.
   - Else `spawn_op(Copy|Cut, items, dest)`.
6. `handle_async` (per R4, R9):
   - `OpProgress` → `processbar.update_progress` (already wired).
   - `OpDone` → `processbar.finish(op, if cancelled {Cancelled} else {Successful},
     None)`; remove the `OpRecord`; Copy/Cut post-op: if `clipboard_ui.is_cut()`
     → `clipboard_ui.reset(false)` (Go `PasteOperationMsg`); mark all panels
     dirty (R9).
   - `OpFail` → keep the process-bar entry in-operation (R4), keep the
     `OpRecord`, `spf_error.open(err, remaining)`. (Skip/abort wiring lands in
     Task 3; until then the modal's skip/abort keys just close the modal.)
7. Tests: `validate_paste_operation` (all three errors + happy path),
   `count_files`, `unique_target` new format (including `(N)` resume), worker
   integration test in a temp dir (copy dir tree → per-file progress totals;
   same-device cut → rename; clipboard cut-reset post-op via a small
   App-level helper if feasible).

## Task 2: Delete (trash + permanent) and Create

Files: `src/app.rs`, `src/fileops.rs` (+ tests).

1. `fileops.rs`:
   - `trash_available_for(path) -> bool`: XDG trash root exists, `path` is not
     inside a known trash dir, and `path` shares `st_dev` with the trash root
     (approximates Go `trash_linux.Available` / `selectTrashDir`).
   - `spawn_worker` gains a `use_trash: bool` parameter (used by `Delete` only;
     `Delete` then calls `delete_item(item, use_trash)`).
   - `create_item` gains Go parity per `createItem` + `checkFileNameValidity`:
     reject `.` / `..` and names ending in `/.` or `/..` (error string
     `"file name cannot be '.' or '..'"` for the first case); non-directory
     path goes through `unique_target` (R1 format) before creating parents +
     file; directory path (`ends_with('/')`) → `create_dir_all`.
2. Delete trigger (Go `getDeleteTriggerCmd`), both select mode and browser
   mode:
   - guards: select mode && `selected_count()==0` → return; browser mode &&
     panel empty → return.
   - `use_trash = has_trash() && trash_available_for(panel.location) && !perm`;
     trash → `Notify::open(TrashWarnTitle, TrashWarnContent, ConfirmAction::Delete)`,
     else permanent strings → `ConfirmAction::PermanentDelete`.
3. Notify confirm (existing `TODO(task-B)` at the `ConfirmAction::Delete` arm):
   items = select mode ? `selected_paths()` : `[focused item location]`;
   `spawn_op(Delete, items, dest=location unused)` with `use_trash`;
   cancel path (notify Cancel) → no op (Go: modal cancel just closes).
4. Delete post-op effect (Go `DeleteOperationMsg`): on `OpDone` for a Delete op
   → `focused panel.reset_selected()` (in addition to R9 dirty).
5. Create (Go `getCreateCmd`/`createOperation`): typing-modal `Confirm` arm →
   close modal, `spawn_op(Create, [location.join(typed)], location)` with
   processbar name `base(typed)`, total 1. Validation failures surface through
   the worker `Fail` → `OpFail` → spf-error modal (Go parity).
6. Tests: trigger wording (trash vs permanent, incl. `trash_available_for`
   cross-device false), delete worker (permanent fallback in temp dir),
   create validation (`.`/`..`/`/.`), create duplicate → `name(1)` format,
   reset_selected on delete completion (App-level or panel-level).

## Task 3: Compress, Extract, skip/abort, gating, cleanup

Files: `src/app.rs`, `src/fileops.rs` (+ tests), `STATUS.md`.

1. Compress trigger (Go `getCompressSelectedFilesCmd`, per R6, R10):
   - guard panel empty → return; sources = select-mode selected (visible order)
     if `selected_count() > 0` else `[focused]`; `first = sources[0]`
     (Go `GetFirstSelectedLocation` == first selected in visible order).
   - zip name from `base(first)` per `getZipArchiveName`: strip last extension
     + `.zip`; if base starts with `.` and contains exactly one `.` →
     `base + ".zip"`; empty base → `notify("Invalid zip target name", err, NoAction)`.
   - validate sources (Go `validateCompressOperation`): each exists; files
     readable (`File::open`); total = `count_readable_files` (recursive count
     of readable non-directories; on count error →
     `notify("Invalid file/dir to compress", err, NoAction)`).
   - target = `panel.location / zipName`; if it exists → `processbar.add` +
     immediate `finish(Cancelled, Some("File already exists"))` (R6).
   - else spawn a Compress worker: per-file progress while walking sources
     (`CurrentFile = base(path)`, `Done++` per entry written, total from
     validation); `fileops::compress_items` takes the explicit target path
     (remove its internal `unique_target`/`compress_archive_name` naming).
2. Extract trigger (Go `getExtractFileCmd`, per R7, R8):
   - guard panel empty → return; gate via `is_extractable` aligned to Go's set
     `{zip, bz, gz, iso, rar, 7z, tar}` (last extension, lowercase).
   - output dir = Go `FileNameWithoutExtension` semantics (strip all trailing
     dot-suffixes, keep leading dot) + `unique_target` (R1 format);
     `create_dir_all`; spawn Extract worker (total 1, name = archive base).
   - `fileops::extract_archive` accepts/returns the explicit dest dir.
3. Skip/abort (Go `spfErrorModelOpenKey` + `FileListErrorState`, per R3–R5):
   - `Skip`: `remaining = spf_error.close()`; if `remaining.is_empty()` →
     abort path; else `fileops::spawn_remaining(kind, op_id, remaining, dest,
     cancel, tx, done_offset)` (extend signature per R5), re-bridge the
     forwarder, keep the process-bar entry in-operation.
   - `Abort`: set `cancel`, `processbar.finish(op, Failed, Some(err))`, remove
     the `OpRecord`. Store the error message from `OpFail` in the `OpRecord`.
4. Quit gating: verify quit flow uses `processbar.has_running()` to require
   confirmation (STATUS.md says already wired — confirm and fix if not).
5. Cleanup: remove every remaining `TODO(task-B)` marker in `src/`; dead-code
   warnings for Task-B APIs must be gone; update `STATUS.md` (component table +
   test count); final gates: `cargo check`, `cargo check --tests`, `cargo test`
   all green.
6. Tests: skip-resumes-remaining (worker over a list where item 1 fails →
   skip → rest complete → process Successful, R3), abort → Failed with message,
   compress duplicate → Cancelled `"File already exists"`, compress naming
   (`.gitignore` → `.gitignore.zip`), extract gate list (`.xz` NOT extractable,
   `.tar.gz` extractable), extract output naming (strip-all-suffixes rule).

## Verification against the Go project

After Task 3: build the Rust binary (`cargo build`), then a manual parity
smoke session in a scratch directory comparing against the Go binary
(`cd /home/flip/repos/superfile && go build -o /tmp/spf-go .` or its documented
build path): same key presses for copy/cut/paste/delete/create/compress/extract,
compare process-bar wording/progress, error modal wording, skip/abort behavior,
and duplicate-name formats. Record results in `STATUS.md`.
