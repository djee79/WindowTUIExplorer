# sentier — a Windows-first terminal file explorer

> Working name. *Sentier* = trail/path. Rename freely.

## What this is

A keyboard-driven TUI file explorer for **Windows 11**, run inside **WezTerm + PowerShell**.
It exists because Yazi and Superfile fall short on the things an engineering-software admin
does all day:

1. **Network shares are first-class.** `\\server\share\...` paths can be bookmarked, browsed,
   and jumped to like any local folder. A dead share never freezes the UI.
2. **Copy jobs you can trust.** Large copies (e.g. AVEVA Dabacon project folders, many GB,
   thousands of files) show exactly what is happening: current file, phase, retries, locked
   files, and a final report. No more "stuck at 99%, no idea why".
3. **A built-in launcher with first-class pins.** Ported from the `wtm` app launcher
   (github.com/djee79/windowstilingmanager, `src/bar.rs` + `src/config.rs`): fuzzy search,
   frecency, run `.exe` / `.bat` / `.lnk`, per-project AVEVA `evars.bat`, and **web apps that
   open as chromeless app windows** (`--app=URL`). Folders and paths are pinned into the same
   store, so one search finds an app, a web app, or a share to jump to.
   It works anywhere a terminal works, with no window manager required.
4. **Fast everywhere.** Rust, multi-threaded, with Telescope-style lazy pickers: results
   stream in while you type, matching runs on worker threads, and only visible rows are
   rendered.

## Non-goals (v0.1)

- Linux/macOS support. Windows only, and development happens on Windows. Keep platform calls
  in `src/platform/` so a port stays possible later, but don't write cfg-gated Linux code
  now.
- Plugins or scripting.
- Archive browsing, git status, video playback (v0.2+).
- Global hotkeys. This is a terminal app; it only sees keys while focused.

## Stack

| Concern | Crate |
|---|---|
| TUI | `ratatui` + `crossterm` |
| Async runtime | `tokio` (multi-thread) |
| Win32 / shell | `windows` (windows-rs) |
| Config | `serde` + `toml` |
| Recycle Bin | `trash` |
| Fuzzy pickers | `nucleo` (Helix's matcher: multi-threaded, incremental, streaming injection) |
| Recursive walk | `ignore` (`WalkParallel`, the ripgrep/fd walker) |
| Content search | `grep-searcher` + `grep-regex` + `grep-matcher` (ripgrep's crates) |
| Glob / regex | `globset`, `regex`, `fancy-regex` (lookbehind/backrefs, PowerRename's "Boost" mode) |
| Secrets in memory | `zeroize` |
| JSON (`machines.source_cmd`) | `serde_json` |
| Clipboard | `arboard` |
| Terminal images | `ratatui-image` (kitty / sixel / iTerm2 protocols, halfblocks fallback) |
| Image decode | `image` (png, jpg, gif, webp, bmp, tiff, ico) + `kamadak-exif` for photo metadata |
| PDF render | WinRT `Windows.Data.Pdf` via `windows` (features `Data_Pdf`, `Storage`, `Storage_Streams`); built into Windows 10+, no DLL to ship |
| Shell thumbnails | `IShellItemImageFactory` via `windows` (`Win32_UI_Shell`): any file type Windows can thumbnail |
| Zip | `zip` (deflate, streamed) |
| Dates / templates | `chrono` |
| Logging | `tracing` + `tracing-appender`, to a file only |
| Errors | `anyhow` in the binary, `thiserror` in modules |

## Architecture rules (non-negotiable)

1. **No blocking I/O on the UI thread. Ever.** Every filesystem call (read_dir, metadata,
   canonicalize, exists) goes through a worker task. The UI sends a request and gets back an
   event through an `mpsc` channel. The UI loop only draws and handles input.
2. **Every FS request has a timeout and can be cancelled.** Default: 3 s for listing and
   metadata on network paths, 500 ms on local paths. On timeout, the pane shows
   `⚠ unreachable (timed out)`, and the user can retry with `R`. Navigating away
   cancels in-flight requests for the old directory.
3. **Paths:** one module, `src/fs/path.rs`, owns normalization:
   - Accept `\\server\share`, `\\server\share\dir`, `C:\`, `C:\dir`, `\\?\C:\...`,
     `\\?\UNC\server\share\...`, and forward-slash variants.
   - The display form never shows `\\?\`. Internally, add the `\\?\` / `\\?\UNC\` prefix when
     a path exceeds 260 characters.
   - **DFS namespaces** (`\\corp.example\dfs\...`) are UNC paths like any other for browsing.
     Wherever the *real* server matters (the `on:` filter, the in-use check, error 1219
     handling), resolve the active target with `NetDfsGetClientInfo`. The status bar shows
     `DFS → \\realserver\share`.
   - **The parent of `\\server\share` is a virtual "server" node** (lists that server's
     shares through `NetShareEnum`, if reachable). The parent of the server node is the
     **Locations** root. Never compute parents by string-splitting on `\`.
   - Table-driven unit tests for all of the above are mandatory.
4. **No `unwrap()` / `expect()` outside tests.** A bad path or a permission error is a
   message in the status bar, not a panic.
5. **The terminal must always be restored.** Install a panic hook that leaves raw mode and
   the alternate screen before printing the panic.
6. Never write to stdout/stderr while the TUI is running. Log to
   `%LOCALAPPDATA%\sentier\logs\`.
7. **Threading model:**
   - **UI thread:** input + draw only. Redraw when state is dirty, capped at ~60 fps, with a
     spinner tick only while something is in flight.
   - **tokio multi-thread runtime:** FS requests, network probes, job supervision.
   - **Blocking pool** (`spawn_blocking`): Win32 calls that can stall (WNet*, NetShareEnum,
     ShellExecute).
   - **`ignore::WalkParallel`** for recursive walks. **nucleo's own pool** for matching.
   - **robocopy** runs as child processes, with stdout read on a dedicated task.
   - State shared between threads goes through channels. No `Mutex` held across an `.await`,
     and none ever touched from the UI thread in a way that can block.
8. **Pickers are lazy:** sources push items into a `nucleo::Injector` as they're found. The
   UI asks for a snapshot each frame and renders only the visible window. Typing never
   waits for a source to finish. Large directories (50k+ entries) in the main view use the
   same approach: sort/filter off-thread, render only the viewport.

## Module layout

```
src/
  main.rs            arg parsing (--cwd-file, start path), runtime + terminal setup/teardown
  app.rs             App state, event loop, key dispatch
  event.rs           AppEvent enum (input, fs results, job updates, ticks)
  ui/                one file per widget: columns.rs, locations.rs, jobs.rs, launcher.rs,
                     prompt.rs, statusbar.rs, help.rs
  fs/
    path.rs          normalization + parent/child logic (see rule 3)
    worker.rs        listing/metadata worker with timeouts + cancellation
    ops.rs           rename, mkdir, delete-to-recycle-bin
  locations.rs       Locations root: pinned folders, drives, saved shares
  pins.rs            the single store for pinned folders, files, apps, web apps; reachability probe
  pin_backups.rs     pins.toml backups (one per editing session, newest 5) and `:pins restore`
  picker/
    mod.rs           generic Telescope-style picker: nucleo + preview pane + actions
    sources.rs       files (WalkParallel), dirs (frecency), pins, launcher, commands, grep
  search/
    walk.rs          parallel walker (WalkParallel; FindFirstFileExW fast path on UNC)
    query.rs         query parser: fuzzy / glob / re:, ext: size: mtime: type: depth: filters
    name.rs          name matching (nucleo, globset, regex)
    content.rs       grep-searcher based content matching + match positions for preview
    results.rs       virtual results folder: batching, live re-filter, sort modes
    index.rs         optional per-share listing index (stretch)
  frecency.rs        shared frecency store for dirs + launcher entries
  help.rs            g? content from the live keymap: sections, chips, filter, column layout
  jobs/
    mod.rs           Job model, JobManager, job events
    robocopy.rs      robocopy backend: spawn, parse output, exit-code mapping
    native.rs        native backend for small copies (single files < 256 MB)
    preset.rs        saved sync/backup jobs: preview (dry run), run, last-run state
    zip.rs           staging + zip writer + verify + retention
    inuse.rs         "who has this project open" (NetFileEnum on the file server)
    report.rs        final job report + per-job log file
  rename/
    engine.rs        search/replace, regex (regex / fancy-regex), match-all, case, apply-to
    tokens.rs        ${...} enumeration + random strings, $date vars, $EXIF vars, sentier extras
    plan.rs          preview plan: collisions, invalid names, case-only and cycle renames
    apply.rs         two-phase apply, journal, undo
    presets.rs       saved rename presets + history
  open/
    rules.rs         open.toml: openers, rules, glob matching, placeholders
    run.rs           inline (suspend TUI) / detached spawning, built-in openers
    assoc.rs         SHAssocEnumHandlers, IAssocHandler::Invoke, SHOpenWithDialog
  machines/
    reach.rs         TCP 445 probe, last-seen tracking
    users.rs         logged-on users (NetWkstaUserEnum) + C$\Users fallback
    templates.rs     [[machine.path]] templates, {user} {host} expansion
    sources.rs       recent, pinned, inventory CSV, external command (getad), AD (stretch)
    audit.rs         log of changes made on admin shares
  auth/
    prompt.rs        masked in-terminal credential prompt
    wnet.rs          WNetAddConnection2W / WNetCancelConnection2W / WNetOpenEnum, error mapping
    credman.rs       CredReadW / CredWriteW (Windows Credential Manager)
    profiles.rs      [auth.*] profiles
  launcher/
    entry.rs         LauncherEntry (schema compatible with wtm)
    rank.rs          ranking + frecency (ported from wtm)
    run.rs           ShellExecuteW + %ENV% expansion (ported from wtm)
    startmenu.rs     scan Start Menu .lnk files (ported from wtm)
  platform/          all `windows` crate calls: drives, WNet*, NetShareEnum, ShellExecute
  config.rs          config.toml load + defaults
```

## Features: v0.1

### 1. Navigation

- Miller columns (parent | current | preview). The preview shows the selected directory's
  listing, a text/image/PDF preview, or file metadata (see section 11).
- Keys are **vim-first**. The full map is in the "Keymap (vim-first)" section. The short
  version: `hjkl`, counts, `gg`/`G`, `Ctrl-d`/`Ctrl-u`, `/` search with `n`/`N`, `H`/`L`
  history, `'` jumps, Space as leader with a which-key popup.
- **Relative line numbers, neovim-style** (`number` + `relativenumber`): the current column
  has a 3-cell gutter. The cursor row shows its **absolute** position (left-aligned, accent),
  and every other row shows its **distance** from the cursor (right-aligned, dimmed). So the
  count for `5j`, `3dd` or `V4j` can be read off the screen. The same gutter appears in
  search results, the jobs list and in picker results while in picker normal mode.
  `numbers = "hybrid" | "relative" | "absolute" | "off"` in config (default `hybrid`).
  `:set rnu` / `:set nornu` / `:set nu` toggle it live.
- **Counts everywhere:**
  - motions: `5j`, `12k`, `3Ctrl-d`, `{n}G`, `:{n}`
  - operators: `3yy` yanks 3 items from the cursor, `5dd` trashes 5, `2x` cuts 2
  - visual: `V4j`
  The pending count shows in the status bar (`5`), like vim's `showcmd`.
- `Enter` / `l` opens (directory → enter, file → its first opener, section 13; `O` = open-with menu).
- Typing a path: `:cd <path>` (or `:e <path>`, `g Space`) accepts any path form from rule 3,
  `~`, and Windows' variables: `%APPDATA%`, `%TEMP%\x`, PowerShell's `$env:APPDATA`
  (unset names stay as typed: `50% off` is a folder name). Tab completes folders, and
  `%APP` + Tab the variable's name. Completion also runs in the worker. Variables work in
  pins, presets and the CLI too.
- Status bar: path, item count, selection count, free space (async), share reachability,
  and the sort order.
- **Sorting** (Yazi's `,` keys): `,n` name (natural), `,m` date modified, `,c` date
  created, `,s` size, `,e` extension; the capital reverses (`,M` oldest first). Folders
  always first, ties in name order. One order for every folder, kept in
  `%LOCALAPPDATA%\sentier\ui.toml` (saved once the keys settle). `:sort <key> [reverse]`.
- **Tabs** (Yazi's idea, vim keys): `t` opens a tab on the current folder, `Alt-1`…`Alt-9`
  or `{n}gt` go to tab n, `gt` / `gT` next / previous, `q` closes a tab (and quits on the
  last one). `:tabnew [path]`, `:tabnext [n]`, `:tabprevious`, `:tabclose`. Plain digits
  stay counts. Each tab keeps its own folder, cursor, `H`/`L` history, `''`, selection
  and filter; listings, jobs, pins and search results are shared. Up to 9 tabs; the header
  lists them (` 1 HIT  2 Downloads `) once there are two.

### 2. Locations root (`gl`, or `h` from a drive root)

A virtual top-level listing with three sections:
- **Pinned folders**, each with a reachability dot: ● reachable, ○ unknown, ✕ unreachable.
- **Drives**: local volumes plus mapped drives. Mapped drives show their UNC target through
  `WNetGetConnectionW` (e.g. `P:  → \\srv01\aveva`).
- **Shares**: saved `\\server\share` roots (a share is added the first time you browse it).

Reachability is probed in the background when the root opens and every 60 s while it's
visible. The probe is a `metadata()` call with a 2 s timeout.

### 3. Pins (bookmarks and launcher entries are one thing)

There is **one store**: `%APPDATA%\sentier\pins.toml`. A pinned folder *is* a bookmark *is* a
launcher entry. The difference is only its `kind`, which decides what Enter does:

| kind | Enter does | Example |
|---|---|---|
| `folder` | navigate the explorer there | `\\srv01\aveva\projects` |
| `file` | ShellExecute "open" (`Ctrl-v`: reveal in explorer) | a spec `.xlsx` |
| `app` | ShellExecute command + args in `dir` | `evars.bat`, `wezterm-gui.exe` |
| `web` | open `url` as an app window in the chosen browser | Grafana, the Forma hub |
| `sync` | run a saved copy job (preview first), see section 9 | HIT server → local |
| `backup` | run a saved backup-to-zip job, see section 9 | HIT → dated zip |

```toml
[[pin]]
kind  = "folder"
name  = "AVEVA projects"
path  = '\\srv01\aveva\projects'
key   = "a"            # optional: 'a jumps straight there
tags  = ["aveva", "acme", "servers"]

[[pin]]
kind    = "web"
name    = "grafana"
url     = "https://grafana.mycorp.com"
browser = "edge"       # edge | chrome | brave | <path to exe>; default edge (always on Win11)
tags    = ["acme", "monitoring"]

# optional per-tag settings
[tags.aveva]
color = "#7fa6f5"
[tags.autodesk]
color = "#e3a857"
```

- **Pin from anywhere (vim marks):**
  - `m{key}` pins the current directory under that key, like setting a vim mark.
  - `M` pins the item under the cursor (folder → `folder`, `.exe`/`.bat`/`.lnk` → `app`,
    other file → `file`).
  - From the launcher, "+ pin URL…" asks for a URL and a browser.
  - Each starts with a name prompt, then tags (with completion), then an optional key.
- `'{key}` jumps or launches directly, like jumping to a vim mark. Pause after `'` and the
  which-key popup lists every pin key. `''` returns to the previous directory (as in vim).
- A key is **one or two** letters or digits (`'h`, `'dl`); `m{key}` sets one-letter keys,
  the key prompt either. A letter is a key *or* starts two-letter keys, never both, so
  `'d` never has to wait and guess: with `'dd` and `'dl`, `'d` waits for the second
  letter (the popup narrows to them). Giving a key that clashes (`'dd` while `'d` is
  `D:\`) asks for the other pin's new key first, with a free one suggested; `Esc` saves
  nothing. The CLI refuses a clash instead.
  `<leader>p` opens the pin picker.
- In any picker, `Ctrl-e` edits, `Ctrl-x` (normal mode: `D` or `dd`, as in the file view) deletes (with confirmation), `Ctrl-t` edits the
  tags. `T` does the same on a pin in Locations.

#### Tags and filtering

Pins can have any number of **tags** (`aveva`, `autodesk`, `acme`, `servers`, …). Tags are
lowercase, can contain `a-z 0-9 - _ .`, and are stored without the `#`. With hundreds of
paths, tags are the main way to narrow things down.

- **Query syntax, in every picker and in the Locations filter (`f`):**
  - `#aveva` → only pins tagged `aveva`. Tag names match by prefix: `#av` works.
  - `#aveva #servers` → both tags (AND). `#aveva,autodesk` → either (OR).
  - `!#archive` → exclude a tag.
  - Built-in filters that need no tagging:
    - `is:folder` `is:app` `is:web` `is:file` `is:sync` `is:backup`
    - `is:unc` `is:local` `is:unreachable`
    - `on:srv01` → pins on that server, including mapped-drive pins resolved to it
    - `is:machine` `is:online` `is:offline`
  - Everything else in the query is fuzzy text as before:
    `hit #aveva is:folder` = folders tagged aveva whose name matches "hit".
- **Completion:** typing `#`, `is:` or `on:` opens an inline completion list with counts
  (`#aveva 23`, `#autodesk 11`, …). `Tab` accepts.
- **Tag bar:** pickers show a one-line bar of the most-used tags with counts, under the
  prompt. The active filters are highlighted.
  - In picker normal mode, `]t` / `[t` cycle a single-tag filter through the bar.
  - `<leader>t` opens a tag picker to multi-select tags, each row with its pin count.
    `Tab` toggles, `Enter` opens the pins with **any** of the chosen tags
    (`#code,library`); typing `#code #library` in a picker still requires both.
- **Saved views:** `:view save <name>` stores the current query (e.g.
  `#aveva on:srv01`) in `pins.toml` as `[[view]]`. It shows up in the launcher as
  `[view]` and can have a `'` key, so `'v` can open "all AVEVA paths on the main server".
- **Tag management:**
  - `:tags` lists every tag with its count.
  - `:tag rename <old> <new>` and `:tag delete <tag>` apply across all pins, after a preview.
  - Per-tag `color` (optional) is used for the tag chips.
- Empty-query launcher: entries are grouped under their **first** tag (headers), with daily
  drivers (frecency) on top.
- Editing happens **in the app**. The user should never have to edit TOML by hand
  (though hand edits must still load).
- **Backups** (`pin_backups.rs`): before sentier writes `pins.toml` (the app or the CLI),
  the file as it was goes to `%APPDATA%\sentier\backups\pins-YYYYMMDD-HHMMSS.toml`, at
  most one per 10 minutes, so each backup is "before that editing session". The newest
  5 are kept. `:pins restore` (also a launcher row) lists them; the preview says which
  pins come back, go away or change. Enter asks, then backs up the current file
  (always) and puts the chosen one in place. A backup that doesn't load can't be
  restored.
- Scale: 1000 pins are ~150 KB of TOML, written in ~1 ms and read and checked in ~4 ms
  (measured 2026-11-09). One TOML file stays fine well past that.
- Opening an unreachable `folder` pin shows a status message and stays put. It never hangs.

### 4. Copy / move jobs

- `yy` yank (copy), `x` cut, `p` paste into the current dir, `P` paste and overwrite (with
  confirmation). Selection: `Tab` toggles the item under the cursor and moves down, `v`/`V`
  enter visual mode (range select with motions), `Esc` clears. Operators work on the
  selection, or on the cursor item when nothing is selected. `.` repeats the last paste in
  another directory.
- **A copy pasted where its original is** gets a free name, like Yazi: `report.pdf` →
  `report_1.pdf`, then `_2`; folders too (`HIT_1`). When `p` finds names the destination
  already holds (copied or cut), a popup asks once for all of them: `k` / Enter keep
  both (the new ones get a number), `o` overwrite (files replaced, folders merged, not
  asked again), `s` skip them, `Esc` paste nothing. `P` skips the popup and replaces,
  after its own confirmation. Files that get a new name copy natively at any size
  (robocopy can't rename a file); with folders in the same paste, that's two jobs. A cut
  pasted where it is does nothing.
- Every paste creates a **Job**, shown in the job panel (`J` toggles it; it opens
  automatically for jobs > 0.5 s).
- **Backend choice:** any directory, or any file ≥ 256 MB → robocopy. Small file-only
  selections → native backend.
- **robocopy backend** (`jobs/robocopy.rs`):
  - Base args: `/E /J /R:0 /W:0 /BYTES /FP /NP /TEE /MT:n /UNILOG+:<job log>` (+ `/MOV`
    for moves of files, `/MOVE` for directories). These defaults are about speed: `/Z` made
    copies crawl (≈5 MB/s instead of > 100 MB/s), so it is **opt-in** per preset with
    `restartable = true`. `/J` is unbuffered I/O. `n` is `jobs.robocopy_threads`
    (default 8, 1–128). The `/L` dry runs run without `/MT`.
  - **Output under `/MT /NP`** (verified on Windows 11): no per-file %, and each file's
    line is written when that file is **done**, its `ERROR` block (if any) right after it.
    So a file counts as copied the moment its line arrives, and an error for it taken
    straight back out. Progress = the `/L` dry run's totals (files, bytes) advanced by each
    finished file; the panel's %, MB/s and ETA come from that. With no in-file progress,
    one large file shows no movement until it's done (the stall banner says so).
  - Parse stdout into events: file copied (path, size), retry / locked
    (`ERROR 32 ... being used by another process` → `Locked`), errors (any `ERROR n (0x...)`
    line → captured with the Windows message), summary table. The serial form (file
    started, then `%` lines) is still parsed, for `/L` and robocopy run by hand.
  - **stdout is drained on a dedicated thread** that only reads the pipe into an unbounded
    channel, so robocopy never blocks on a full pipe. Parsing happens on the job's thread,
    and UI updates are **coalesced**: at most one batch per 50 ms (~20/s), runs of finished
    files merged into one (counts summed, the latest file wins), only the last `%` kept.
    The UI can never slow robocopy down.
  - **Exit codes are a bitmask:** 0 nothing copied, 1 files copied, 2 extra files, 4
    mismatches, 8 some copies failed, 16 fatal. ≥ 8 is a failure. Map these to a clear
    final state; never show raw numbers alone.
  - Keep fixture files of real robocopy output, captured with sentier's own arguments
    (`/MT:8 /NP /J`): success, nothing to copy, many files (thread interleaving),
    restartable (`/Z`), mirror with extras, `/L`, locked file, access denied, missing
    source, unreachable dest. They live in `tests/fixtures/robocopy/`; unit-test the parser
    against them.
- **Job panel shows:** source → dest, phase (`Scanning` / `Copying` / `Retrying` / `Finishing`
  / `Done` / `Failed` / `Cancelled`), **current file path**, bytes + files done/total,
  throughput, ETA, elapsed time, and a live error list. Layout per docs/JOBS_PANEL_SPEC.md:
  RUNNING cards / FINISHED entries on the left; a stage stepper with each stage's time, a
  wide bar, a 64 s THROUGHPUT chart beside STATUS, PATHS and the ACTIVITY tail on the right.
- **"Finishing" is an explicit phase.** When bytes are 100 % but the process hasn't exited,
  say so (`Finishing: flushing to network…`) with an elapsed timer. This is the
  "stuck at 99 %" case. It must never look frozen.
- **Final report:** counts (copied / skipped / failed / extra), the list of failed files with
  reasons, and a key to open the full log (`o`). Reports persist in
  `%LOCALAPPDATA%\sentier\jobs\` until dismissed; dismissing deletes the job's log too.
- **Self-cleaning:** at startup (off the UI thread) sentier deletes job logs older than
  `jobs.keep_logs_days` (default 30, `0` keeps all) unless a listed report still points
  to them. It also deletes staging folders untouched for a day (a backup killed midway;
  not sooner, because a scheduled `sentier run` may be using one).
  The app log rotates daily (7 kept), `logs\openers.log` moves to `openers.old.log` past
  1 MB, and the rename journal keeps 100 batches. `audit.log` is never trimmed: it's
  the audit trail.
- `c` cancels a job (kills robocopy; the report says which files were done).
  `r` retries failed files only. `p` pauses (robocopy is stopped and holds no locks) and
  resumes (the step runs again: copied files skipped; a half-copied file starts over, or
  resumes where it stopped when the preset is `restartable`, which adds `/Z`).
  `O` opens the job's source / destination (the places menu, section 9).
- **sentier owns retries** (docs/JOBS_PANEL_SPEC.md): robocopy never waits (`/R:0`), so a
  locked file (error 32/33) doesn't hold the copy up. The locked files are tried again
  after the main pass, every 10 s, up to 5 times. While it waits the panel shows a stall
  banner (what, why, next try) and `s` leaves the file out (the job ends with `!` and
  `N skipped`), `R` tries it now. Any job without progress for 10 s shows the banner
  with the most specific cause, and the status bar says `⚠ 1 job stalled`.
- **AVEVA guard:** if the source contains Dabacon project markers (configurable glob list,
  default: a directory containing an `evars.bat` alongside `*000` database dirs, and lock
  files), show a one-line warning before starting: "Looks like an AVEVA project. Make sure
  no one has it open." Continue / Abort.

### 5. File operations

- `r` or `cw` renames, with an inline prompt that starts with the cursor before the extension.
  `a` makes a new file, `A` a new directory (a trailing `\` in `a` also makes a directory).
- `dd` deletes to the Recycle Bin (`trash` crate) with a confirmation. `D` permanently deletes
  with a stronger confirmation. Network paths have no Recycle Bin: say so in the confirmation.
- Bulk rename: `r` / `cw` with a selection (or a visual range), or `:rename`, opens the
  PowerRename-style panel (section 16). `:rename-edit` opens the selection's names in
  `$EDITOR` (Neovim), one per line, and applies the edited lines after the same preview.
- **Zip / extract** (`jobs/archive.rs`), jobs in the panel like a copy:
  - `<leader>zc` / `:zip [name]` zips the selection (or the item) into this folder; the
    name is asked, suggested (`report.pdf` → `report.zip`, a folder → its name, several
    → the folder's name). Written as `.partial`, read back (every CRC), then renamed; a
    name in use gets a number.
  - `<leader>zx` / `:unzip` extracts the archive under the cursor here, into a folder
    named by the archive: its one top folder (`aveva.zip` holding `aveva\…` → `aveva`,
    no `aveva\aveva\`), else the archive's name (`HIT.zip` → `HIT\`). The name is read
    first (off-thread); when that folder exists, a popup asks: `m` merge into it (files
    with the same name replaced, nothing else touched, never removed on a failure), `n`
    a new folder (`aveva_1`, the default for Enter), `Esc` cancel. A `.zip` extracts
    straight into the folder (no rename at the end, which Windows refuses while
    antivirus has new files open). `.zip` natively, every CRC checked; an entry that
    would land outside the folder refuses the archive.
    `.7z .rar .tar .gz .tgz .bz2 .xz .cab` through 7-Zip when it's installed (a
    `.tar.gz` is unpacked both ways), else "needs 7-Zip".
  - Also in the `O` menu ("Compress to zip" / "Extract here"). Admin shares: the usual
    confirmation naming the machine, and the audit log.
- All operations run in the worker and report back. Errors go to the status bar.

### 6. Launcher (ported from wtm)

- **Open with** `<leader><leader>` (Space Space, as in LazyVim). `Ctrl-Space` is an alias when
  the terminal delivers it. Avoid `Alt+Space`: Windows uses it for the window menu.
- The launcher is the picker over **all pins** (every kind), plus Start Menu apps on request.
- **Import from wtm:** on first run, if `%APPDATA%\wtm\config.toml` exists, offer to import
  its `[[launcher]]` entries (`name, command, args, dir, group, key`). wtm's `group`
  becomes the first tag (lowercased). A command
  (`:import-wtm`) does the same any time. Mapping:
  - entries with `args = "--app=URL"` on a browser exe → `kind = "web"`, with `url` and
    `browser` extracted
  - a `command` that is a directory → `folder`
  - everything else → `app`
  - wtm's `key` (a global hotkey) is dropped. In sentier, `key` is the `'`-jump key.
- **Web apps:** launch `<browser exe> --app=<url>`, the same as wtm. Resolve browser exes
  from the registry (`App Paths`) instead of hard-coded paths. Optional `profile` field →
  `--profile-directory=<profile>`.
- **Ranking:** nucleo does the matching. Keep wtm's tiers on top of the nucleo score: name
  prefix > tag prefix > substring > fuzzy. Tag/`is:`/`on:` filters apply first (they
  narrow the set, they don't score). Within a tier, sort by frecency, then alphabetically.
  With an empty query, show first-tag headers with daily drivers on top.
- **Frecency** (port from wtm `usage_score`): `%LOCALAPPDATA%\sentier\usage.tsv`, keyed by
  what the pin opens (`command + args`, `url`, or normalized `path`, lowercased), so renames
  keep history. The same store also records every directory visited (zoxide-style) for the
  dirs picker. Score = min(count, 100) × recency
  weight (< 1 h: 8, < 1 d: 4, < 7 d: 2, else 1).
- **Run** (port from wtm `run_command`): `ShellExecuteW("open", command, args, dir)` with
  `ExpandEnvironmentStringsW` on each field. If nothing matches the query, Enter runs the
  typed text directly.
- **In-app management:** the same pin actions as section 3. Changes save immediately.
- **Start Menu scan** (port from wtm `scan_start_menu`): lists per-user, then all-users
  `.lnk` files, deduped by name; toggle entries in or out of the launcher.
- **Context actions (explorer-specific):** when the current directory contains `evars.bat`,
  the status bar shows `⏎E  open E3D project here`, and the launcher pins a
  "Run evars.bat in <dir>" entry at the top.

### 7. Pickers (Telescope-style)

One generic picker widget (`picker/`): prompt on top, results list, preview pane on the
right (directory listing / first N lines of text / metadata). It's lazy per rule 8.
Results appear as soon as the first ones are found. A counter shows `matched / scanned`,
with a spinner while the source is still producing.

| Key | Picker | Source |
|---|---|---|
| `<leader><leader>` | Launcher | all pins + frecency |
| `<leader>p` | Pins | pins only |
| `<leader>ff` | Files | `WalkParallel` from cwd |
| `<leader>fr` | Recent dirs | frecency store (zoxide-like) + subdirs of cwd |
| `<leader>/` | Grep | ripgrep crates, from cwd |
| `<leader>:` | Commands | every action, with its key shown (doubles as help) |
| `<leader>sk` | Keymaps | every binding in every mode, searchable |

- **Telescope behaviour:** pickers open in insert mode (typing filters). `Ctrl-j`/`Ctrl-k`
  or `Ctrl-n`/`Ctrl-p` move. `Esc` switches to normal mode (`j`/`k`, `gg`/`G`, `Ctrl-d`/
  `Ctrl-u` scroll the results, `i` back to insert). `Esc` or `q` in normal mode closes.

- The Files picker on a **network path** walks with a depth limit (default 4) and shows
  `network walk, depth 4`. `Ctrl-l` lifts the limit. Respect `.ignore` files. Skip
  `$RECYCLE.BIN` and `System Volume Information`.
- Picker actions: `Enter` open, `Ctrl-o` open-with menu (section 13), `Ctrl-v` reveal in
  Explorer, `Ctrl-y` copy path, `Ctrl-a`
  pin, `Tab` multi-select (then `Enter` yanks them into a copy job).

### 8. Path utilities

- `yp` copies the full path of the selection. `yP` copies it **UNC-resolved** (`P:\x` →
  `\\srv01\aveva\x`, via `WNetGetUniversalNameW`). That's the form to paste into Teams or an
  email for someone without the same drive mappings. `yn` copies the name only.
- `gx` opens the current directory in Windows Explorer (fallback for shell things we don't
  do). `gw` opens a new WezTerm tab there (`wezterm cli spawn --cwd`). `go` opens it in the
  configured editor.
- `Alt+Enter` shows Windows Properties (`SHObjectPropertiesW`).

### 9. Project jobs: saved sync and backup presets

The admin chore this tool exists for: refresh a local working copy of a Dabacon project
from the server on demand, and take zipped backups, without retyping paths or babysitting
Explorer. Presets are pins (section 3), so they show up in the launcher and pickers, can
have a `'` key, and get frecency like everything else.

#### Sync preset

```toml
[[pin]]
kind    = "sync"
name    = "HIT → local"
tags    = ["aveva", "acme"]
key     = "h"
source  = '\\srv01\project\HIT'
dest    = 'D:\aveva\local\HIT'
mode    = "mirror"            # mirror | update
exclude_dirs  = []            # robocopy /XD
exclude_files = []            # robocopy /XF
include       = []            # only these names in source (folders end in \): the selection
check_in_use  = true          # see "In-use check" below
restartable   = false         # true adds robocopy /Z: resumable, but many times slower
```

- `mode = "update"` → `robocopy /E /J /MT:n ...` copies new and changed files and never
  deletes anything in `dest`.
- `mode = "mirror"` → `robocopy /MIR /J /MT:n ...` makes `dest` an exact copy, **deleting
  local files that aren't on the server**. Because that's destructive, mirror always runs
  the preview first.
- `restartable = true` adds `/Z` (section 4): a file cut off by a pause or a dropped link
  resumes where it stopped instead of starting over. Worth it only for huge files on a
  flaky link; it costs most of the speed.
- **Preview (dry run) first:** Enter on a sync pin runs robocopy with `/L` (list only) and
  shows a summary before anything is touched: `412 files to copy (3.8 GB) · 17 newer on
  server · 6 local files will be DELETED (mirror)`. The list can be expanded per category.
  Then `Enter` runs it, `Esc` cancels. `Shift-Enter` (or `:sync! <preset>`) skips the preview for `update` mode
  only; mirror never skips it.
  In the list, `x` leaves the file under the cursor out from now on and `X` its folder
  (`/XF` / `/XD` with the full path, so it's neither copied nor, in mirror mode, deleted);
  the dry run is redone at once, on the same list and row. `E` brings everything back.
- **Making one, by going there (pick mode):** `Space n s` / `:sync new` in the source
  folder; with a selection (`Tab` / `v`), only those are synced (`include`: each folder its
  own robocopy step, mirrored within itself; the files one more step, copied). Then the
  header turns into a `PICK` banner: move around as usual (`hjkl`, `'` jumps, `gl`, `:cd`,
  `A` for a new folder) and press `p` in the destination ("paste the sync here"); a folder
  inside the source (or vice versa) is refused there and then. The short questions follow
  (mode), the name last, suggested from both folders (`HIT → HIT-local`), then tags and
  key. `Esc` while picking saves nothing. `Ctrl-e` on a preset picks its folders the same
  way, starting where they are. Backups: the zips folder is picked the same way.
- The real run is a normal job (section 4): same panel, Finishing phase, report, log.
- **Places menu:** `O` (picker normal mode) or `Ctrl-o` on a sync / backup preset, and
  `O` on a job in the jobs panel, open a menu of its two folders: Source, Destination, or
  Both (source here, destination in a new tab) in sentier; the same in Explorer; or copy
  either path. A backup's destination is its zips folder.
- **Last-run state** is stored in `%LOCALAPPDATA%\sentier\presets.json`: time, outcome,
  bytes, duration. The pin row shows `last: today 08:12 ✓ 3.8 GB in 2m41s` or
  `last: failed (2 locked)`.
- **Reverse direction is not offered.** Pushing local → server is out of scope on
  purpose, so a preset can't overwrite the production project by mistake. `source` must
  never equal or contain `dest`, and vice versa; refuse to save otherwise.

#### Backup preset

```toml
[[pin]]
kind      = "backup"
name      = "HIT backup"
tags      = ["aveva", "acme", "backup"]
source    = '\\srv01\project\HIT'
dest_dir  = 'E:\backups\aveva'                  # chosen folder; may be a UNC path
file_name = "{name}_{date:%Y-%m-%d_%H%M}.zip"   # {name} = last path component of source,
                                                # {date:…} strftime, {host} this PC, {user} Windows user
keep      = 10                                  # retention: keep the newest N; 0 = keep all
compression = "default"                         # store | fast | default | best
check_in_use = true
restartable = false                             # true: staging uses robocopy /Z (slow)
```

- **Zip name:** asked in the app after the zips folder is picked (and on `Ctrl-e`),
  pre-filled with the default; the line under it shows the name a run would write now
  (`→ HIT_2026-11-10_1430.zip`) or what's wrong. Retention matches `{date}` as `*` but
  `{host}` / `{user}` literally, so a backup never deletes zips another PC or user made.
- **Pipeline:**
  1. In-use check (optional).
  2. Robocopy the source into a local **staging** dir
     (`%LOCALAPPDATA%\sentier\staging\<job>`), so the zip is built from a stable local
     snapshot, not by reading thousands of files over SMB one at a time.
  3. Zip the staging dir to `<dest_dir>\<file_name>.partial`.
  4. **Verify:** reopen the archive, check the entry count matches staging, and CRC-check
     every entry.
  5. Rename `.partial` → final name (so a half-written zip never looks like a valid
     backup).
  6. Apply retention: delete the oldest zips matching this preset's pattern beyond `keep`.
     Never touch files that don't match the pattern.
  7. Delete staging.
- The job panel shows each phase as a step (`staging 1/5`, `zipping 2/5`, `verifying 3/5`,
  …) with progress inside each.
- If an optional 7-Zip install is found (`7z.exe` on PATH or in Program Files), offer it as
  a faster multi-threaded zip backend (`compression` maps to `-mx`). The built-in `zip`
  crate stays the default and must always work alone.
- Needs free space for staging + zip. Check both before starting and refuse with a clear
  message if there isn't enough (estimated from the source size the robocopy `/L` pass
  reports).
- When `source` is a pinned `sync` dest (e.g. `D:\aveva\local\HIT`), skip staging and zip
  directly. That copy is already local.

#### In-use check (both presets)

- When the source is on a file server, call `NetFileEnum(server, basepath = <share-relative
  project path>)` to list open files and the users holding them. This requires admin
  rights on the file server (the user is an admin). Show: `3 files open by jdoe, asmith.
  Continue / Abort`.
- If the call is denied or the server can't be queried, say so in one line and fall back to
  the configurable lock-marker globs (`aveva.project_markers`). Never block the job just
  because the check couldn't run.

#### Running presets without the TUI

- `sentier run "<pin name>"` runs a sync/backup preset headless: no preview, `update`
  semantics enforced for sync unless `--allow-mirror` is passed. It writes the same log and
  report and returns robocopy-style exit codes (0 ok, 8+ failure). This lets Windows Task
  Scheduler run nightly backups with no extra tooling.

### 10. Shell integration

- `--cwd-file <path>`: on `Q`, write the cwd there. Ship wrappers in `shell/`:
  - PowerShell: `function s { $f = New-TemporaryFile; sentier --cwd-file $f @args; $d = Get-Content $f; if ($d -and $d -ne $PWD) { Set-Location -LiteralPath $d }; Remove-Item $f }`
  - fish: equivalent function, for later.
- `sentier <path>` starts in that path (any form from rule 3).
- **CLI for other tools** (hyprctl-style, like wtm), so scripts and other TUIs (`getad`,
  `cadcommands`) can feed it:
  `sentier pin <path|url> [--name N] [--tag T]...`, `sentier pins [--query "#aveva is:unc"] --json`,
  `sentier open <pin name>`. These edit `pins.toml` safely (atomic write + reload
  notification), even while a TUI instance is running.


### 11. Previews: images and PDFs

The preview column (and the picker preview pane) renders **real images** in the terminal.

- **Protocol:** `ratatui-image` picks the best one the terminal supports (kitty graphics,
  iTerm2 inline images, sixel) and falls back to **halfblocks** (`▀` with truecolor), which
  works everywhere. Cell pixel size comes from the terminal query. If that fails, use the
  `preview.cell_px = [w, h]` config value.
- **⚠ Verify first (Windows-specific risk):** under WezTerm on Windows, output passes through
  ConPTY. Before building anything else for previews, write `examples/image_probe.rs`:
  draw one PNG with each protocol and record which ones actually display in WezTerm and in
  Windows Terminal. Pick the default per terminal from those results. Halfblocks must always
  work.

#### What gets previewed

| File | How | Shown with it |
|---|---|---|
| png jpg gif webp bmp tiff ico | `image` crate decode → resize to the pane | dimensions, format, size. Photos: EXIF date, camera, GPS yes/no. |
| PDF | WinRT `Windows.Data.Pdf`: `PdfDocument::LoadFromFileAsync` → page → `RenderToStreamAsync` at the pane's pixel size | page `n / N`, page size in mm and inches **plus the named sheet size when it matches** (A0–A4, ANSI A–E, Arch C–E), PDF title |
| anything else Windows can thumbnail (dwg, docx, xlsx, mp4, heic…) | `IShellItemImageFactory::GetImage` (thumbnail only) | "Windows thumbnail" + metadata. Depends on installed thumbnail handlers (e.g. Autodesk's for DWG). |
| text / code (`.bat` `.ps1` `.vb` `.pmlmac` `.log`…, and files with no or an unknown extension that hold no zero bytes) | the first 64 KB as lines with numbers (UTF-8, UTF-16 with or without BOM, Windows-1252); `zp` full screen, `J`/`K` `^d`/`^u` scroll. Syntax colours: `syntect`, v0.2 | encoding, line count |
| binary with no thumbnail | metadata only | as today |

#### Keys while the cursor is on a previewable file

- `]p` / `[p` next / previous PDF page, `{n}]p` jumps n pages, `gP` goes to a page number.
- `zp` toggles a **full-screen preview** (the preview takes the whole body; `Esc` or `zp`
  returns). In full screen, `]p`/`[p` still page and `+`/`-` zoom by changing the render
  scale.
- `gx` opens the file in its default app (as for any file).

#### Performance rules (rule 1 applies: nothing on the UI thread)

- Decode, PDF render and shell thumbnails run on the blocking pool. A cursor move cancels
  the in-flight render (generation counter: stale results are dropped). A single step
  (the cursor rested ≥ 80 ms before it) asks for its preview at once. While keys stream in
  (holding `j`), wait for stillness first (40 ms for a listing or text, 60 ms for a
  render), so not every file passed is read.
- **Cache:**
  - in memory: an LRU of rendered images keyed by `(path, mtime, size, page, pane px size)`,
    ~64 entries
  - on disk: `%LOCALAPPDATA%\sentier\cache\thumbs\`, for network paths only, capped at
    500 MB, oldest first
- **Preload:** after the current file renders, preload the next and previous file (Yazi's
  idea). The PDF preview also renders the next page in the background.
- **Network guard:** on UNC paths, files over `preview.max_network_mb` (default 50 MB) don't
  auto-preview. The pane says `large file on network: zp to preview`. Shell thumbnails of
  network files have a 3 s timeout like every network call.
- Show a spinner with `rendering page 2…` while waiting. Never leave the previous file's
  image on screen for a different file.

### 12. Search: Yazi-style, results become a folder

**Must-have.** `s` (names) and `S` (contents) search recursively from the current directory.
**Results stream into the main view as a virtual folder**, the way Yazi does it, not into a
popup. Everything you can do in a folder works on the results: move with `j`/`k`, preview
(images, PDFs), `Tab`/`v` select, `yy`/`x`/`dd`, `yp`/`yP`, `M` pin, `l` opens the file or
enters the result's folder, `gx`.

#### Name search (`s`)

- The prompt accepts:
  - **fuzzy** text (default; nucleo, smart-case): `b1001 iso` matches
    `hitiso\B-1001_rev3.pdf`, so related files are found from fragments in any order
  - **glob**, when the query has `*` `?` `[`: `*.pdf`, `B-10??_rev*`; commas mean "or":
    `*.pdf, *.txt` (also `ext:pdf, txt`)
  - **regex**, with a leading `re:` prefix
- Filters can be mixed in:
  - `ext:pdf,dwg`, `type:d` / `type:f`
  - `size:>50mb`, `mtime:<7d`
  - `depth:3`, `hidden:yes`
  - `!old` excludes path segments
- Fuzzy matches the **relative path**, and hits in the file name score higher (fzf /
  Telescope `find_files` behaviour), so `b1001 iso` finds `hitiso\B-1001_rev3.pdf`. Globs
  match the file name, unless they contain `\`.

#### Content search (`S`)

- ripgrep's crates (`grep-searcher` + `grep-regex`), smart-case, literal by default (`re:`
  for regex).
- Results list each matching file **with its match count**. The preview pane jumps to the
  first match with surrounding lines highlighted. `]m` / `[m` step through matches in the
  preview.
- Binary files are skipped; `.pdf` text isn't searched in v0.1.
- Same filters as name search (`ext:pmlmac,pmlfrm`).

#### Why it's fast, and stays responsive

- The walk is `ignore::WalkParallel` (the fd/ripgrep engine) across all cores. Matching
  happens inside the walker threads. Hits go to the UI in batches about every 16 ms through
  a channel, so the results list grows while you watch, and you can browse the first results
  while the search continues.
- The header shows live progress:
  `search  "b1001 iso"  in \\srv01\project\HIT   ● 48 213 scanned · 312 found · 2.1 s`.
  `Esc` cancels; the found results stay as a folder.
- Re-typing the query (`s` again, prefilled) re-filters already-found entries **instantly**
  while a fresh walk runs, so refining feels instant even on a slow share.
- **Network shares:** directory enumeration over SMB is the bottleneck, not matching. So:
  - Use **`FindFirstFileExW` with `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`** in the
    walker on UNC paths. It returns name + size + mtime + attributes in one round trip and
    fetches large batches. **Benchmark** it against `std::fs::read_dir` on a real share
    (`examples/enum_bench.rs`) and keep the faster one.
  - Run more walker threads on UNC paths than local (latency-bound, not CPU-bound). The
    default is 16; configurable.
  - Skip `$RECYCLE.BIN`, `System Volume Information`, and anything matched by
    `search.exclude` globs.
  - Never follow junctions or symlinks by default (loops). `follow:yes` turns it on.
- **Optional index for pinned network folders** (v0.1 stretch goal): `:index add <path>`
  keeps a compact on-disk listing (path, size, mtime) of a big share like
  `\\srv01\project\HIT` in `%LOCALAPPDATA%\sentier\index\`. Name search on an
  indexed path answers **from the index first, instantly**, then a live walk confirms and
  updates it. Each sync preset's dry run refreshes the index of its source for free (it
  enumerates the tree anyway).

#### Navigation around results

- `Esc` (with no search running) or `H` leaves the results and returns to the folder you
  searched from. `L` goes forward to them again (results are part of the history).
- `gr` jumps to the real parent folder of the result under the cursor.
- Sorting: by relevance (score) while typing; the folder sort keys work on results too:
  `,n` path, `,m` / `,c` dates, `,s` size, `,e` extension (capital: reversed), `,r` back to
  relevance.
- `<leader>ff` (files picker) and `<leader>/` (grep picker) remain as Telescope-style popups
  for quick open-and-close. `s`/`S` are the browse-the-results versions. Both share the same
  walker and matcher code.

### 13. Open with (Yazi's `O`)

`o` (or `Enter`, or `l` on a file) runs the **first** opener that matches the file. `O` opens a
small **menu of every opener that matches**, the way Yazi does, plus the apps Windows itself
has registered for that extension. It works on the selection too, not just the cursor item.

#### The menu

```
╭─ Open B-1001_rev3.pdf with ─────────────────────╮
│ configured  (*.pdf)                             │
│  1  Default app            Adobe Acrobat     o  │
│  2  Bluebeam Revu                     ★ last    │
│  3  Reveal in Explorer                    gR    │
│  4  Copy UNC path                         yP    │
│ Windows suggests  (registered for .pdf)         │
│  5  Microsoft Edge                              │
│  6  Foxit PDF Reader                            │
│  7  Open with… (Windows dialog)                 │
│  8  Properties                                  │
╰─ 1-9 pick · j/k Enter · type to filter · Esc ───╯
```

- Pick with `1`–`9`, `j`/`k` + `Enter`, or by typing (fuzzy filter).
- `★ last` marks the opener used last time for this extension. With
  `open.remember = true` (the default), a recently used opener moves to the top of the
  menu for that extension. It never changes what `o` does unless you set
  `open.o_uses_last = true`.
- **"Windows suggests"** comes from `SHAssocEnumHandlers(ext, ASSOC_FILTER_RECOMMENDED)`:
  the same list as Explorer's "Open with" submenu, so installed apps appear with no config.
  Running one calls `IAssocHandler::Invoke` on the selected file(s).
- **Open with… (Windows dialog)** → `SHOpenWithDialog`, the real Windows chooser, including
  "always use this app".

#### Openers and rules: `%APPDATA%\sentier\open.toml`

```toml
# Openers: what can run. Placeholders: {path} {paths} {dir} {name} {stem} {ext}
[opener.nvim]
desc  = "Neovim (this terminal)"
run   = ["nvim", "{paths}"]
mode  = "inline"        # inline: suspend the TUI, run in this terminal, come back after

[opener.nvim-tab]
desc  = "Neovim (new WezTerm tab)"
run   = ["wezterm", "cli", "spawn", "--cwd", "{dir}", "--", "nvim", "{path}"]
mode  = "detached"

[opener.notepadpp]
desc  = "Notepad++"
run   = ['C:\Program Files\Notepad++\notepad++.exe', "{paths}"]
mode  = "detached"      # detached: start and forget (GUI apps)
for   = "all"           # one process for the whole selection ("each" = one per file)

[opener.vscode]
desc  = "VS Code"
run   = ["code", "{paths}"]
mode  = "detached"

[opener.revu]
desc  = "Bluebeam Revu"
run   = ['C:\Program Files\Bluebeam Software\Bluebeam Revu\Revu.exe', "{path}"]
mode  = "detached"

# Rules: which openers are offered, in order (first = what `o` does). First matching rule wins.
[[open.rule]]
match = "*.{pml,pmlmac,pmlfrm,pmlfnc,pmldat,mac,txt,md,toml,json,log,bat,cmd,ps1,csv,xml,ini}"
use   = ["nvim", "notepadpp", "vscode", "nvim-tab", "reveal"]

[[open.rule]]
match = "*.pdf"
use   = ["default", "revu", "reveal", "copy-unc"]

[[open.rule]]
match = "*.{dwg,dxf}"
use   = ["default", "reveal", "copy-unc"]

[[open.rule]]
match = "dir"           # directories
use   = ["enter", "explorer", "terminal-here", "nvim"]

[[open.rule]]
match = "*"
use   = ["default", "notepadpp", "reveal"]
```

- **Built-in openers** (always available, no config needed):
  - `default` → `ShellExecuteW "open"`
  - `editor` → the editor (config.toml `editor`, else `$VISUAL` / `$EDITOR`, else `nvim`):
    terminal editors (and `code --wait`) run inline, GUI ones detached. Offered in every
    `O` menu.
  - `notepadpp` (Notepad++) and `vscode` (VS Code): GUI editors, offered for files when
    installed (App Paths / PATH / `Program Files\<name>`), hidden when not; usable in rules.
  - `reveal` → `explorer.exe /select,"<path>"`, so Explorer opens with the file selected
  - `openwith` → the Windows dialog
  - `properties` → `SHObjectPropertiesW`
  - `explorer` → Explorer on the folder you're looking at, with the item under the cursor
    selected (`/select`): what sentier shows. `gx` opens the current folder itself.
  - `terminal-here` → `wezterm cli spawn --cwd` in the folder you're looking at (`gw`: the
    current folder)
  - `copy-path` / `copy-unc`
  - `enter` → navigate into a directory
- **Modes:**
  - `inline`: leave the alternate screen and raw mode, run the program in this console,
    wait for it, then restore and refresh the listing. For Neovim, less, etc.
  - `detached`: start without waiting (GUI apps). Its stdout/stderr go to the log, never the
    TUI. sentier sets `ELECTRON_NO_ATTACH_CONSOLE=1` for itself at startup, so Electron apps
    started through Windows' own handlers (the "Windows suggests" apps, the default app)
    don't attach to its console and print their logs over the TUI (VS Code did).
  - `tab`: an alias for `detached` with a WezTerm-spawn helper.
- `{paths}` expands to one argument per selected file. `for = "each"` runs the opener once
  per file instead. Network paths are passed as UNC, never re-mapped.
- Executables are resolved through PATH and `App Paths` (like the launcher). A missing exe
  greys the opener out in the menu with "not found", instead of failing on run.
- **Any pin of kind `app` can be used as an opener** (`use = ["pin:E3D — HIT (local)"]`),
  so the launcher and the menu share one list of apps.
- **Without open.toml** the built-in rules apply: folders enter; text and code (`*.{txt,md,
  toml,json,log,ps1,bat,rs,py,…,pml,pmlmac,pmlfrm,…}`) open in the editor with `o` (Notepad++
  / VS Code next in `O`); everything else opens in its Windows app (editors in `O`). Nothing
  has to be configured for the common cases.
- Bad rules or openers are reported in the status bar at startup, never a crash.

### 14. Machines (user PCs through admin shares)

Admins open `\\COMPUTERNAME\C$` to troubleshoot files on a user's machine. Machines are
**pins of `kind = "machine"`**, so tags, search, frecency and `'` keys all work, but they get
their **own section and behaviour** because they don't act like folders. There are many of
them, they're often offline, and what you want is usually a path *inside* them.

```toml
[[pin]]
kind  = "machine"
name  = "JDOE-LT042"
host  = "JDOE-LT042"
user  = "jdoe"          # optional hint for {user}; otherwise detected
auth  = "admin"         # auth profile (section 15); machines default to "admin"
tags  = ["users", "laptop", "piping"]
key   = ""
```

- **Where they show up:**
  - a **MACHINES** section in Locations
  - `gm` opens the Machines root
  - `<leader>m` opens the machine picker
  - typing any `\\HOST\X$` admin-share path (`:cd \\JDOE-LT042\C$`) goes through the machine
    logic automatically
- **Adding one by name:** `a` in `gm` (the Machines list) asks for a PC name (`JDOE-LT042`,
  an IP, or `\\JDOE-LT042\C$` pasted as is); in `Space m`, typing a name nothing matches
  and pressing Enter opens it; a "+ Add a machine…" row there and in the launcher asks too.
  Each opens the machine view (probed first) and makes it a recent machine; `M` / `m{key}`
  pin it.
- **Recent machines are automatic:** every machine you open is remembered for
  `machines.recent_days` (default 30), then drops off unless you pin it (`M`). This keeps the
  pin list clean.
- **Reachability before SMB:**
  - a TCP connect to port 445 with a 500 ms timeout runs **before** any SMB call, so an
    asleep laptop shows `offline · last seen 3 d ago` immediately instead of a 30 s hang
  - `R` re-probes
  - `machines.watch = true` re-probes visible machines every 60 s
- **Opening a machine** shows a **machine view** instead of the raw `C$` root:
  - status: online/offline, logged-on user(s), last seen
  - the **path templates**
  - `C$` itself as the last entry
- **Logged-on user:** `NetWkstaUserEnum` (level 1, requires admin) off-thread with a
  timeout. Fallback: list `C$\Users` sorted by last modified and pre-select the most recent
  non-system profile. The user can always pick another one (`u` in the machine view).
- **Path templates**, the main feature. One key jumps to the same troubleshooting folder on
  any machine:
  ```toml
  [[machine.path]]
  name = "Desktop"
  path = 'C$\Users\{user}\Desktop'
  key  = "d"
  [[machine.path]]
  name = "Downloads"
  path = 'C$\Users\{user}\Downloads'
  key  = "w"
  [[machine.path]]
  name = "User temp"
  path = 'C$\Users\{user}\AppData\Local\Temp'
  key  = "t"
  [[machine.path]]
  name = "Roaming"
  path = 'C$\Users\{user}\AppData\Roaming'
  key  = "r"
  [[machine.path]]
  name = "ProgramData"
  path = 'C$\ProgramData'
  key  = "p"
  [[machine.path]]
  name = "Windows temp"
  path = 'C$\Windows\Temp'
  ```
  `{user}` and `{host}` expand. A template whose folder doesn't exist shows greyed out with
  "missing" (checked lazily, with a timeout). Add AVEVA/Autodesk-specific ones freely.
- **Finding a machine you haven't visited** (`<leader>m`, then type), from several sources
  merged in the picker:
  - recent and pinned machines
  - an optional inventory CSV: `machines.inventory_csv` with `host,user,...` columns, e.g.
    an export from PDQ or AD, so you can **search by person** (`jdoe` → `JDOE-LT042`)
  - an optional external command that prints JSON (`machines.source_cmd`), e.g. the
    author's `getad` tool
  - (stretch) a direct AD query through ADSI (`IDirectorySearch`,
    `(&(objectCategory=computer)(name=*q*))`)
- **Safety on admin shares** (any `\\host\X$` path):
  - the header turns **red** and reads `ADMIN SHARE · JDOE-LT042 · jdoe`
  - every change (paste, rename, delete, new file) asks for confirmation **naming the machine
    and user**: `Delete 2 files on JDOE-LT042 (jdoe's machine)? Permanent: no Recycle Bin
    over the network.`
  - `machines.audit = true` (default) appends every change to
    `%LOCALAPPDATA%\sentier\audit.log`: time, host, user, operation, paths
- **Machine actions** (`O` on a machine, also usable as openers):
  - Open `C$`
  - RDP: `mstsc /v:{host}`
  - Computer Management: `compmgmt.msc /computer=\\{host}`
  - Event Viewer: `eventvwr {host}`
  - Remote PowerShell in a new WezTerm tab: `pwsh -NoExit -c Enter-PSSession {host}`
  - `remotectl {host}`
  - Copy hostname
  All of these are configurable like any opener (section 13).

### 15. Credentials

Most of the time **there is no prompt**. Windows sends the current login, like Explorer does.
The prompt appears only when Windows refuses.

- **Profiles** name an account, never a password:
  ```toml
  [auth.admin]
  user = 'ACME\adm-user'
  ```
  Pins and machines can set `auth = "admin"`. Machines default to `admin`; shares default to
  the current login.
- **Flow:**
  1. Access fails with `ERROR_ACCESS_DENIED` (5) or `ERROR_LOGON_FAILURE` (1326), or the
     target uses a profile with no live connection.
  2. **A masked prompt inside the terminal**:
     - `User` is pre-filled from the profile (editable)
     - `Password` is shown as `●`, and bracketed paste is allowed (password managers)
     - `[ ] Remember in Windows Credential Manager`, **unchecked by default**
     - `Tab` / `Shift-Tab` move between fields, `Space` toggles the checkbox,
       `Enter` connects, `Esc` cancels
  3. Connect with `WNetAddConnection2W` to the requested share (e.g. `\\JDOE-LT042\C$`),
     `CONNECT_TEMPORARY`, no drive letter. The connection lasts for the Windows logon
     session, so **robocopy jobs, other sentier instances and Explorer reuse it**.
  4. A wrong password re-prompts with the reason shown (`logon failure: wrong user name or
     password`), up to 3 tries.
- **"Remember"** writes a `CRED_TYPE_DOMAIN_PASSWORD` entry with `CredWriteW` for that host.
  Option: "for all machines in ACME", using a `*.domain` wildcard target. Windows then uses it
  automatically, with no prompt next time, in sentier and elsewhere. sentier never stores a
  password itself.
- **Error 1219** (`ERROR_SESSION_CREDENTIAL_CONFLICT`: already connected to that server as
  another user). Explorer just fails here; sentier offers the two real fixes:
  - `d`: disconnect the existing connection(s) to that server (`WNetCancelConnection2W`),
    then connect as the profile user
  - `a`: connect through an **alias** of the same server (FQDN or IP), which Windows treats as
    a different server
- **Other errors** are mapped to plain messages:
  - 53 → path not found / offline
  - 1311 → no logon server available
  - 86 → wrong password
  - 1909 → account locked
- **Hygiene:**
  - the password lives in a `zeroize`d buffer, is wiped right after the call, and is never
    logged, written to TOML, put on a command line or shown in the audit log
  - the prompt never echoes
- **Managing connections:**
  - `:net list` shows live SMB connections and their user (`WNetOpenEnum`,
    `RESOURCE_CONNECTED`)
  - `:net drop <server>` disconnects
  - `:net drop-all` disconnects everything sentier opened

### 16. Bulk rename (PowerRename-style)

`r` (or `cw`) with a selection or a visual range opens a full-screen rename panel on those
items (Yazi's idiom; `r` on a single item without a selection stays the inline rename).
`:rename` opens it too: on the selection, or on the cursor item when nothing is selected.
`R` keeps its meaning (retry / refresh / re-probe). **The syntax is PowerRename's**, so
existing habits and saved patterns carry over, and sentier adds what a terminal tool can do
better (real undo, collision checks, presets).

#### Layout

- **Top:** two input lines, `Search for` and `Replace with`. They're vim-aware prompts
  (insert/normal), with history (`Ctrl-p`/`Ctrl-n`) and completion for `${` and `$` tokens.
- **Middle:** a row of option toggles showing their state.
- **Body:** a live preview table with columns `Original │ Renamed │ status`. Changed parts are
  highlighted, and items are numbered with the relative gutter. It updates on every keystroke,
  computed off-thread for big lists.

#### Options (PowerRename parity)

Toggled in panel normal mode (`Esc` from a field):

| Key | Option | Notes |
|---|---|---|
| `r` | Use regular expressions | Rust `regex`, close to ECMAScript. `$1`, `$2`, `${name}` captures in Replace. |
| `b` | Extended regex ("Boost" mode) | switches to `fancy-regex`: lookbehind, backreferences |
| `a` | Match all occurrences | off = first match only, like PowerRename |
| `c` | Case sensitive | off by default |
| `n` | Apply to: name only · extension only · name + extension | cycles |
| `f` `d` `s` | Include files · folders · subfolders' contents | subfolders walk with the search walker |
| `t` | Text formatting: none · lowercase · UPPERCASE · Title case · Capitalize Each Word | cycles |
| `e` | Enumerate items (`${...}` counters) | |
| `x` | Random string values | |
| `m` | Date source: file creation time · modified time · EXIF date taken | creation time is PowerRename's default |
| `o` | Show only items that change (PowerRename's "Renamed" header filter) | |
| `Space` | Include/exclude the item under the cursor (PowerRename's Original-column checkbox) | |

#### Replace tokens (same syntax as PowerRename)

- **Enumeration** (with `e`): `${}` (starts at 0), `${start=X}`, `${increment=X}`,
  `${padding=X}`, combined with `;` (e.g. `${padding=4;increment=2;start=10}`). Several
  counters can appear in one string. Items are numbered **in the order shown** (the current
  sentier sort), so the preview is the truth.
- **Random** (with `x`): `${rstringalnum=X}`, `${rstringalpha=X}`, `${rstringdigit=X}`,
  `${ruuidv4}`.
- **Date/time** of the date source: `$YYYY $YY $Y $MMMM $MMM $MM $M $DDDD $DDD $DD $D`
  `$hh $h $mm $m $ss $s $fff $ff $f`. Month and weekday names follow the Windows user locale.
- **EXIF/XMP photo metadata:**
  - camera: `$CAMERA_MAKE $CAMERA_MODEL $LENS $ISO $APERTURE $SHUTTER $FOCAL $FLASH`
    `$EXPOSURE_BIAS $ORIENTATION $COLOR_SPACE`
  - image: `$WIDTH $HEIGHT $AUTHOR $COPYRIGHT`
  - GPS: `$LATITUDE $LONGITUDE $ALTITUDE`
  - date taken: `$DATE_TAKEN_YYYY $DATE_TAKEN_YY $DATE_TAKEN_MM $DATE_TAKEN_DD`
    `$DATE_TAKEN_HH $DATE_TAKEN_mm $DATE_TAKEN_SS`
  - Files without EXIF produce an empty value, flagged in the status column.
- **sentier extras:**
  - `${parent}`: the parent folder's name
  - `${name}`: the original stem
  - `${ext}`: the original extension
  - `${n}` with `reset=folder`: a counter that restarts in each folder when subfolders are
    included
  - `${pdf_title}`: from PDF metadata (via the preview code)
- A `$` that isn't a token stays literal. `$$` is a literal `$`.

#### Safety checks (status column; Apply is blocked while any ✕ remains)

- **Collisions:** two items mapping to the same name, or a target that already exists and
  isn't part of the batch → `✕ collides with …`.
- **Invalid names:** `< > : " / \ | ? *`, control characters, reserved names
  (`CON PRN AUX NUL COM1… LPT1…`), a trailing space or dot, an empty name, or a full path
  over 260 characters (allowed, flagged `⚠ long path`).
- **Case-only renames** (`b-1001.pdf` → `B-1001.pdf`) work despite Windows being
  case-insensitive: they go through a temporary name.
- **Swaps and cycles** (`a→b`, `b→a`): the two-phase apply handles them. Everything is first
  renamed to unique temporary names, then to the final names.
- **Unchanged** items are marked `·` and skipped.

#### Apply, undo, presets

- `Enter` in normal mode → `Rename 37 items? (2 skipped)` → apply. Each rename uses
  `MoveFileExW`. Failures (locked file, access denied) are listed per item; the others still
  go through. Admin-share rules from section 14 apply (confirmation names the machine, audit
  log).
- **Real undo:** every batch is written to a journal
  (`%LOCALAPPDATA%\sentier\rename-journal\`). `u` in the file view (the newest batch in
  that folder or below), or `:undo-rename [n]` (the n newest anywhere), reverts whole
  batches, newest first, after a preview, as long as the renamed files still exist. This
  goes beyond PowerRename, which relies on Explorer's single Undo.
- **Presets:** `Ctrl-s` saves the current search, replace and all toggles under a name
  (`[[rename.preset]]` in `rename.toml`), e.g. "ISO rev bump" or "photos by date taken".
  `Ctrl-o` opens the preset picker. The last-used options are restored when the panel opens
  (PowerRename's "restore flags on launch").
- **History:** the last 50 searches and replaces are kept per field (PowerRename's "recently
  used strings" / autocomplete).

## Keymap (vim-first)

The author lives in Neovim (LazyVim) and Yazi. Bindings follow vim first, then LazyVim
conventions, then Yazi where vim has no opinion. **Every binding is remappable.**

### Modes

`normal` (default) · `visual` (range select) · `picker.insert` / `picker.normal`
(Telescope-style) · `input.insert` / `input.normal` (prompts: rename, `:` command line,
filters) · `dialog` (confirmations, sync preview).

### Normal mode

| Keys | Action |
|---|---|
| `h` `j` `k` `l` / arrows | parent · down · up · open. Counts work: `5j`, `3k` (read them off the relative-number gutter). |
| `{n}yy` `{n}dd` `{n}x` | operators take counts: n items from the cursor |
| `gg` `G` `{n}G` | top · bottom · line n |
| `Ctrl-d` `Ctrl-u` / `Ctrl-f` `Ctrl-b` | half page / full page down · up |
| `zz` `zt` `zb` | scroll cursor to middle · top · bottom |
| `/` `?` then `n` `N` | incremental search in the current dir (forward/backward), next/prev match |
| `f` | filter the current dir (live, `Esc` clears): plain text, or the search language: `*.pdf`, `*.pdf, *.txt` (commas = or), `ext:pdf,dwg`, `size:>10mb`, `mtime:<7d`, `type:d`, `!old`, `re:…` |
| `s` · `S` | recursive name search · content search; results stream in as a folder (section 12) |
| `gr` · `,r` | (in results) go to the result's real folder · sort by relevance |
| `,n` `,m` `,c` `,s` `,e` (capital: reversed) · `:sort <key> [reverse]` | order the folder by name · date modified · date created · size · extension; folders first; remembered (ui.toml); the status bar shows it (`sort: size ↓`) |
| `H` `L` / `Ctrl-o` | back / forward in directory history (`Ctrl-o` = back) |
| `-` | parent directory (vim-vinegar / oil.nvim) |
| `~` | home · `gl` Locations · `gm` Machines · `gd` Downloads · `g\` the root of the current drive or share |
| `'{key}` `''` | jump to pin · previous directory |
| `m{key}` `M` | pin current dir with key · pin item under cursor |
| `Tab` `v` `V` `Esc` | toggle select + down · visual · visual-line · clear selection |
| `yy` `yp` `yP` `yn` | yank files · copy path · copy UNC-resolved path · copy name |
| `x` `p` `P` | cut · paste · paste overwrite |
| `dd` `D` | Recycle Bin · permanent delete (both confirm) |
| `r` / `cw` · `a` · `A` | rename (with a selection: the bulk rename panel, section 16) · new file · new dir |
| `u` | undo the last bulk rename in this folder (section 16) |
| `.` | repeat last paste/operation here |
| `J` | toggle the job panel |
| `g.` | toggle hidden/system files |
| `]p` `[p` `gP` · `zp` | next / prev / go-to PDF page · toggle full-screen preview (`+`/`-` zoom) |
| `o` / `Enter` · `O` | open with the first opener for this file type · **open-with menu** (section 13) |
| `gR` | reveal in Explorer (selects the file in an Explorer window) |
| `gx` `gw` `go` · `Alt-Enter` | open in Explorer · new WezTerm tab here · editor · Properties |
| `:` | command line (`:cd`, `:e`, `:pin`, `:sync <preset>`, `:backup <preset>`, `:rename`, `:import-wtm`, `:q`) |
| `t` · `Alt-1`…`9` / `{n}gt` · `gt` `gT` | new tab here · go to tab n · next / previous tab |
| `q` / `:q` · `ZZ` · `ZQ` | quit (with several tabs: close this one) · quit writing cwd (for the shell wrapper) · quit without writing |
| `Ctrl-L` / `:redraw` | clear and redraw the screen (if another program wrote on it) |
| `g?` | help overlay (`?` stays vim's backward search), generated from the live keymap (docs/HELP_OVERLAY_SPEC.md): sections by task, key chips, the `:` commands with their keys, `/` filters. `<leader>sk` searches the same rows. |

### Leader (Space), LazyVim-style

| Keys | Action |
|---|---|
| `<leader><leader>` | launcher |
| `<leader>p` | pins picker |
| `<leader>ff` · `<leader>fr` | files picker · recent dirs |
| `<leader>/` | grep picker |
| `<leader>j` | jobs panel |
| `<leader>rs` · `<leader>rb` | run a sync preset · run a backup preset (pickers) |
| `<leader>zc` · `<leader>zx` | zip the selection here · extract the archive under the cursor here |
| `<leader>l` | Locations |
| `<leader>m` | machines picker (recent, pinned, inventory, AD) |
| `<leader>t` | tag picker (multi-select, applies a tag filter) |
| `<leader>:` · `<leader>sk` | commands · keymaps |
| `<leader>e` | open the current dir in `$EDITOR` (Neovim) |

### which-key popup

When a prefix key is pending (`<leader>`, `g`, `y`, `d`, `z`, `'`, `m`) and no second key
arrives within **300 ms** (configurable), show a which-key popup at the bottom listing every
continuation with its description. The `'` popup lists every pin with its key, so it doubles
as a quick pin menu. Pressing a listed key runs the action. `Esc` cancels. Bindings carry a
`desc` so the popup and `<leader>sk` stay in sync with the keymap.

### Prompts (rename, `:` line, filters)

These start in insert mode with readline basics (`Ctrl-w` delete word, `Ctrl-u` clear,
`Ctrl-a`/`Ctrl-e` line start/end, `Ctrl-r Ctrl-w` insert the name under the cursor). `Esc`
switches to normal mode with a small vi subset: `h l w b e 0 $ x`, `i a I A`, `dw cw ciw C D`,
`u`. A second `Esc` cancels. `Enter` confirms in either mode.

### Config: `%APPDATA%\sentier\keymap.toml`

```toml
leader = " "
timeout_ms = 300

[normal]
prepend = [
  { on = "<C-p>", run = "picker files", desc = "Find files" },
  { on = "gh",    run = "cd ~",          desc = "Home" },
]

[picker.insert]
append = [ { on = "<C-l>", run = "picker lift-depth", desc = "Lift network depth limit" } ]
```

Vim key notation everywhere (`<C-d>`, `<S-CR>`, `<leader>`, `<Esc>`). Like Yazi, there are
`prepend` / `append` / full override per mode. Unknown actions or conflicting chords are
reported in the status bar at startup, never a crash.

### Terminal key caveats

- Enable crossterm keyboard enhancement (`PushKeyboardEnhancementFlags`) when the
  terminal supports it. **Verify** under WezTerm on Windows (ConPTY) which of `Ctrl-Space`,
  `Shift-Enter`, `Ctrl-i` vs `Tab` and `Ctrl-m` vs `Enter` arrive as distinct keys.
- Rule: **every binding that needs a modifier the terminal might swallow has a plain-key
  equivalent** (`Ctrl-Space` → `<leader><leader>`, `Shift-Enter` → `:sync! <preset>`).
- `Tab` and `Ctrl-i` are the same key in most terminals, so `Tab` is selection and history
  forward is `L`.

## Config

`%APPDATA%\sentier\config.toml`, every field optional with defaults:
`show_hidden`, `numbers` (hybrid | relative | absolute | off), `timeouts.local_ms`, `timeouts.network_ms`, `jobs.robocopy_threads` (robocopy `/MT:n`, default 8, 1–128), `jobs.keep_logs_days` (30),
`jobs.native_max_bytes`, `aveva.project_markers`, `preview.protocol` (auto | kitty | sixel |
iterm2 | halfblocks), `preview.cell_px`, `preview.max_network_mb`, `preview.cache_mb`,
`theme.key` / `theme.header` (`"#rrggbb"`: key chips and section headers in `g?`).

**Colorschemes:** `:colorscheme <name>` (`:colo`, Tab completes and cycles) switches live
and is remembered in `ui.toml`, read before the first frame: `default`, `tokyonight-night`
(`tokyonight`), `tokyonight-storm`, `tokyonight-moon` (folke/tokyonight.nvim colors, blue
accent, cyan folders). `:colorscheme` alone says which is in use. Every UI color comes from
`ui::theme::pal()`; a new scheme is one more `Palette` in `SCHEMES`.

## Milestones

1. **M1, skeleton:** CI on Windows, terminal setup/teardown + panic hook, event loop, Miller columns over
   local disks through the async worker, relative-number gutter + counts. `path.rs` with full
   tests.
2. **M2, network:** UNC browsing, the server node, Locations root, timeouts and an
   "unreachable" state. Test against a dead host (`\\10.255.255.1\x`) and confirm the UI stays
   responsive.
2a. **M2a, search:** `s` name search + `S` content search as streaming result folders, the
   SMB enumeration benchmark, progress/cancel, history integration. (Moved early: must-have.)
2b. **M2b, previews:** image probe first (protocols under WezTerm/ConPTY), then image +
   PDF + shell-thumbnail previews, cache, preload, `]p`/`[p`/`zp`.
2c. **M2c, credentials:** auth profiles, masked prompt, WNetAddConnection2W, Credential
   Manager, the 1219 fixes, `:net` commands.
3. **M3, pins + picker:** `pins.toml`, the generic nucleo picker, pin picker, `'` jumps,
   in-app edits, reachability dots.
3b. **M3b, machines:** machine pins, recent machines, the 445 probe, machine view with path
   templates, logged-on user, admin-share safety + audit log, machine actions, inventory CSV.
4. **M4, jobs:** robocopy backend + parser + fixtures, job panel, Finishing phase,
   reports, cancel/retry, AVEVA guard. Native backend.
4b. **M4b, project presets:** sync preset with `/L` preview + mirror safeguards, backup
   pipeline (staging → zip → verify → rename → retention), in-use check, last-run state,
   `sentier run`.
4c. **M4c, bulk rename:** the PowerRename-parity engine + token set, live preview, safety
   checks, two-phase apply, journal + undo, presets.
5. **M5, launcher + open-with:** open.toml rules/openers, `o`/`O` menu, Windows-registered
   handlers, inline mode for Neovim. Then the launcher: app/web/file pins, wtm import, frecency, Start Menu scan, context
   actions.
6. **M6, pickers + polish:** files/dirs/commands pickers, path utilities, file ops,
   cwd-file wrappers, CLI, config overrides.

Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` only after the panic
hook has been verified to restore the terminal.

## Definition of done (every milestone)

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` pass.
- Manual check from a real WezTerm + PowerShell session on Windows 11, including one
  real SMB share and one unreachable share.
- No new `unwrap()` outside tests. No blocking call reachable from `app.rs` / `ui/`.

## Development setup

Development happens on **Windows 11** (WezTerm + PowerShell, Rust via rustup).

1. **Pure core, thin platform layer.** Keep logic that doesn't need Win32 free of Win32
   calls, so it can be unit-tested quickly and deterministically: path normalization,
   pin/preset/config parsing, ranking + frecency, the robocopy output parser
   (fixture-driven), exit-code mapping, zip retention logic, the job state machine.
   All Win32 calls live in `src/platform/`.
2. **CI:** GitHub Actions on `windows-latest` runs fmt + clippy + tests on every push.
3. **Capture robocopy fixtures once, at work:** the `examples/capture_robocopy.rs` probe runs
   the scenarios with sentier's own arguments (success, many files, restartable, mirror,
   `/L`, locked file, access denied, missing source, unreachable dest) and writes stdout +
   exit code into `tests/fixtures/robocopy/`.
4. **Dead-share testing:** use a non-routable host (`\\10.255.255.1\x`) or a test share
   whose SMB service you can stop.
5. **Debugging:** `tracing` logs to `%LOCALAPPDATA%\sentier\logs\sentier.log`, with
   `SENTIER_LOG=debug`. Tail it in a second WezTerm pane
   (`Get-Content -Wait -Tail 50`) while the TUI runs. `sentier run` and the CLI work
   headless, so most job bugs can be reproduced without the TUI.
   `SENTIER_LOG=latency=debug` logs one line per key that changed the screen:
   `key_to_frame_ms` (key read → frame handed to the terminal) and `draw_ms` (building
   and writing it).

## Working agreements for Claude Code

- Build milestone by milestone. Don't scaffold empty modules for future milestones.
- When behaviour of a Windows API or robocopy is uncertain, write a tiny probe binary in
  `examples/` and ask the user to run it on Windows, rather than guessing.
- Ask before adding a crate not in the stack table.
