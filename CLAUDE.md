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
| Glob / regex | `globset`, `regex` |
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
  jobs/
    mod.rs           Job model, JobManager, job events
    robocopy.rs      robocopy backend: spawn, parse output, exit-code mapping
    native.rs        native backend for small copies (single files < 256 MB)
    preset.rs        saved sync/backup jobs: preview (dry run), run, last-run state
  open/
    rules.rs         open.toml: openers, rules, glob matching, placeholders
    run.rs           inline (suspend TUI) / detached spawning, built-in openers
    assoc.rs         SHAssocEnumHandlers, IAssocHandler::Invoke, SHOpenWithDialog
    zip.rs           staging + zip writer + verify + retention
    inuse.rs         "who has this project open" (NetFileEnum on the file server)
    report.rs        final job report + per-job log file
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
- Typing a path: `:cd <path>` (or `:e <path>`) accepts any path form from rule 3, with Tab
  completion. Completion also runs in the worker.
- Status bar: path, item count, selection count, free space (async), share reachability.

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
  `<leader>p` opens the pin picker.
- In any picker, `Ctrl-e` edits, `Ctrl-x` deletes (with confirmation), `Ctrl-t` edits the
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
  - Everything else in the query is fuzzy text as before:
    `hit #aveva is:folder` = folders tagged aveva whose name matches "hit".
- **Completion:** typing `#`, `is:` or `on:` opens an inline completion list with counts
  (`#aveva 23`, `#autodesk 11`, …). `Tab` accepts.
- **Tag bar:** pickers show a one-line bar of the most-used tags with counts, under the
  prompt. The active filters are highlighted.
  - In picker normal mode, `]t` / `[t` cycle a single-tag filter through the bar.
  - `<leader>t` opens a tag picker to multi-select tags. `Tab` toggles, `Enter` applies the
    filter.
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
- Opening an unreachable `folder` pin shows a status message and stays put. It never hangs.

### 4. Copy / move jobs

- `yy` yank (copy), `x` cut, `p` paste into the current dir, `P` paste and overwrite (with
  confirmation). Selection: `Tab` toggles the item under the cursor and moves down, `v`/`V`
  enter visual mode (range select with motions), `Esc` clears. Operators work on the
  selection, or on the cursor item when nothing is selected. `.` repeats the last paste in
  another directory.
- Every paste creates a **Job**, shown in the job panel (`J` toggles it; it opens
  automatically for jobs > 5 s).
- **Backend choice:** any directory, or any file ≥ 256 MB → robocopy. Small file-only
  selections → native backend.
- **robocopy backend** (`jobs/robocopy.rs`):
  - Base args: `/E /Z /R:2 /W:5 /BYTES /FP /TEE /UNILOG:<job log>` (+ `/MOV` for moves of
    files, `/MOVE` for directories). `/MT:n` is configurable (default off). **Verify** how
    `/MT` changes stdout (per-file % output may be suppressed) and make the parser handle
    both modes.
  - Parse stdout into events: file started (path, size), file % progress, retry
    (`ERROR 32 ... being used by another process` → `Locked`), errors (any `ERROR n (0x...)`
    line → captured with the Windows message), summary table.
  - **Exit codes are a bitmask:** 0 nothing copied, 1 files copied, 2 extra files, 4
    mismatches, 8 some copies failed, 16 fatal. ≥ 8 is a failure. Map these to a clear
    final state; never show raw numbers alone.
  - Keep fixture files of real robocopy output (success, locked file, access denied,
    unreachable dest, /MT mode) in `tests/fixtures/robocopy/` and unit-test the parser
    against them.
- **Job panel shows:** source → dest, phase (`Scanning` / `Copying` / `Retrying` / `Finishing`
  / `Done` / `Failed` / `Cancelled`), **current file path**, bytes + files done/total,
  throughput, ETA, elapsed time, and a live error list.
- **"Finishing" is an explicit phase.** When bytes are 100 % but the process hasn't exited,
  say so (`Finishing: flushing to network…`) with an elapsed timer. This is the
  "stuck at 99 %" case. It must never look frozen.
- **Final report:** counts (copied / skipped / failed / extra), the list of failed files with
  reasons, and a key to open the full log (`o`). Reports persist in
  `%LOCALAPPDATA%\sentier\jobs\` until dismissed.
- `c` cancels a job (kills robocopy; the report says which files were done).
  `r` retries failed files only.
- **AVEVA guard:** if the source contains Dabacon project markers (configurable glob list,
  default: a directory containing an `evars.bat` alongside `*000` database dirs, and lock
  files), show a one-line warning before starting: "Looks like an AVEVA project. Make sure
  no one has it open." Continue / Abort.

### 5. File operations

- `r` or `cw` renames, with an inline prompt that starts with the cursor before the extension.
  `a` makes a new file, `A` a new directory (a trailing `\` in `a` also makes a directory).
- `dd` deletes to the Recycle Bin (`trash` crate) with a confirmation. `D` permanently deletes
  with a stronger confirmation. Network paths have no Recycle Bin: say so in the confirmation.
- Bulk rename: `:rename` opens the selection's names in `$EDITOR` (Neovim), one per line.
  The edited lines are applied after a preview of the changes.
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
check_in_use  = true          # see "In-use check" below
```

- `mode = "update"` → `robocopy /E /Z ...` copies new and changed files and never deletes
  anything in `dest`.
- `mode = "mirror"` → `robocopy /MIR /Z ...` makes `dest` an exact copy, **deleting local
  files that aren't on the server**. Because that's destructive, mirror always runs the
  preview first.
- **Preview (dry run) first:** Enter on a sync pin runs robocopy with `/L` (list only) and
  shows a summary before anything is touched: `412 files to copy (3.8 GB) · 17 newer on
  server · 6 local files will be DELETED (mirror)`. The list can be expanded per category.
  Then `Enter` runs it, `Esc` cancels. `Shift-Enter` (or `:sync! <preset>`) skips the preview for `update` mode
  only; mirror never skips it.
- The real run is a normal job (section 4): same panel, Finishing phase, report, log.
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
file_name = "{name}_{date:%Y-%m-%d_%H%M}.zip"   # {name} = last path component of source
keep      = 10                                  # retention: keep the newest N; 0 = keep all
compression = "default"                         # store | fast | default | best
check_in_use = true
```

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
| text / code | first N lines, syntax-highlighted (`syntect`, v0.2) | encoding, line count |
| binary with no thumbnail | metadata only | as today |

#### Keys while the cursor is on a previewable file

- `]p` / `[p` next / previous PDF page, `{n}]p` jumps n pages, `gP` goes to a page number.
- `zp` toggles a **full-screen preview** (the preview takes the whole body; `Esc` or `zp`
  returns). In full screen, `]p`/`[p` still page and `+`/`-` zoom by changing the render
  scale.
- `gx` opens the file in its default app (as for any file).

#### Performance rules (rule 1 applies: nothing on the UI thread)

- Decode, PDF render and shell thumbnails run on the blocking pool. A cursor move cancels
  the in-flight render (generation counter: stale results are dropped). Wait ~60 ms of cursor
  stillness before starting a render, so holding `j` doesn't render every file.
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
  - **glob**, when the query has `*` `?` `[`: `*.pdf`, `B-10??_rev*`
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
- Sorting: by score while typing. `,s` cycles through score / path / size / modified.
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
  - `reveal` → `explorer.exe /select,"<path>"`, so Explorer opens with the file selected
  - `openwith` → the Windows dialog
  - `properties` → `SHObjectPropertiesW`
  - `explorer` → opens the folder
  - `terminal-here` → `wezterm cli spawn --cwd`
  - `copy-path` / `copy-unc`
  - `enter` → navigate into a directory
- **Modes:**
  - `inline`: leave the alternate screen and raw mode, run the program in this console,
    wait for it, then restore and refresh the listing. For Neovim, less, etc.
  - `detached`: start without waiting (GUI apps). Its stdout/stderr go to the log, never the
    TUI.
  - `tab`: an alias for `detached` with a WezTerm-spawn helper.
- `{paths}` expands to one argument per selected file. `for = "each"` runs the opener once
  per file instead. Network paths are passed as UNC, never re-mapped.
- Executables are resolved through PATH and `App Paths` (like the launcher). A missing exe
  greys the opener out in the menu with "not found", instead of failing on run.
- **Any pin of kind `app` can be used as an opener** (`use = ["pin:E3D — HIT (local)"]`),
  so the launcher and the menu share one list of apps.
- Bad rules or openers are reported in the status bar at startup, never a crash.

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
| `f` | filter the current dir (live, `Esc` clears) |
| `s` · `S` | recursive name search · content search; results stream in as a folder (section 12) |
| `gr` · `,s` | (in results) go to the result's real folder · cycle sort |
| `H` `L` / `Ctrl-o` | back / forward in directory history (`Ctrl-o` = back) |
| `-` | parent directory (vim-vinegar / oil.nvim) |
| `~` | home · `gl` Locations · `gd` Downloads · `g\` the root of the current drive or share |
| `'{key}` `''` | jump to pin · previous directory |
| `m{key}` `M` | pin current dir with key · pin item under cursor |
| `Tab` `v` `V` `Esc` | toggle select + down · visual · visual-line · clear selection |
| `yy` `yp` `yP` `yn` | yank files · copy path · copy UNC-resolved path · copy name |
| `x` `p` `P` | cut · paste · paste overwrite |
| `dd` `D` | Recycle Bin · permanent delete (both confirm) |
| `r` / `cw` · `a` · `A` | rename · new file · new dir |
| `.` | repeat last paste/operation here |
| `J` | toggle the job panel |
| `g.` | toggle hidden/system files |
| `]p` `[p` `gP` · `zp` | next / prev / go-to PDF page · toggle full-screen preview (`+`/`-` zoom) |
| `o` / `Enter` · `O` | open with the first opener for this file type · **open-with menu** (section 13) |
| `gR` | reveal in Explorer (selects the file in an Explorer window) |
| `gx` `gw` `go` · `Alt-Enter` | open in Explorer · new WezTerm tab here · editor · Properties |
| `:` | command line (`:cd`, `:e`, `:pin`, `:sync <preset>`, `:backup <preset>`, `:rename`, `:import-wtm`, `:q`) |
| `q` / `:q` · `ZZ` · `ZQ` | quit · quit writing cwd (for the shell wrapper) · quit without writing |
| `g?` | help overlay (`?` stays vim's backward search). `<leader>sk` searches every keymap. |

### Leader (Space), LazyVim-style

| Keys | Action |
|---|---|
| `<leader><leader>` | launcher |
| `<leader>p` | pins picker |
| `<leader>ff` · `<leader>fr` | files picker · recent dirs |
| `<leader>/` | grep picker |
| `<leader>j` | jobs panel |
| `<leader>rs` · `<leader>rb` | run a sync preset · run a backup preset (pickers) |
| `<leader>l` | Locations |
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
`show_hidden`, `numbers` (hybrid | relative | absolute | off), `timeouts.local_ms`, `timeouts.network_ms`, `jobs.robocopy_threads`,
`jobs.native_max_bytes`, `aveva.project_markers`, `preview.protocol` (auto | kitty | sixel |
iterm2 | halfblocks), `preview.cell_px`, `preview.max_network_mb`, `preview.cache_mb`.

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
3. **M3, pins + picker:** `pins.toml`, the generic nucleo picker, pin picker, `'` jumps,
   in-app edits, reachability dots.
4. **M4, jobs:** robocopy backend + parser + fixtures, job panel, Finishing phase,
   reports, cancel/retry, AVEVA guard. Native backend.
4b. **M4b, project presets:** sync preset with `/L` preview + mirror safeguards, backup
   pipeline (staging → zip → verify → rename → retention), in-use check, last-run state,
   `sentier run`.
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
   the scenarios (success, locked file, access denied, unreachable dest, `/MT`, `/L`) and
   writes stdout + exit code into `tests/fixtures/robocopy/`.
4. **Dead-share testing:** use a non-routable host (`\\10.255.255.1\x`) or a test share
   whose SMB service you can stop.
5. **Debugging:** `tracing` logs to `%LOCALAPPDATA%\sentier\logs\sentier.log`, with
   `SENTIER_LOG=debug`. Tail it in a second WezTerm pane
   (`Get-Content -Wait -Tail 50`) while the TUI runs. `sentier run` and the CLI work
   headless, so most job bugs can be reproduced without the TUI.

## Working agreements for Claude Code

- Build milestone by milestone. Don't scaffold empty modules for future milestones.
- When behaviour of a Windows API or robocopy is uncertain, write a tiny probe binary in
  `examples/` and ask the user to run it on Windows, rather than guessing.
- Ask before adding a crate not in the stack table.
