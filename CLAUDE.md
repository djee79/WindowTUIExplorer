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

- Linux/macOS support. Windows only. Keep platform calls in `src/platform/` so a port stays
  possible later, but don't write cfg-gated Linux code now.
- Plugins or scripting.
- Image previews, archive browsing, git status (v0.2+).
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
| Content search (v0.2) | `grep-searcher` + `grep-regex` (ripgrep's crates) |
| Clipboard | `arboard` |
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
  frecency.rs        shared frecency store for dirs + launcher entries
  jobs/
    mod.rs           Job model, JobManager, job events
    robocopy.rs      robocopy backend: spawn, parse output, exit-code mapping
    native.rs        native backend for small copies (single files < 256 MB)
    preset.rs        saved sync/backup jobs: preview (dry run), run, last-run state
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

- Miller columns (parent | current | preview of the selected directory, or file metadata).
- Keys: `h j k l`, arrows, `gg` / `G`, `Enter` opens (directory → enter, file → ShellExecute
  "open"), `.` toggles hidden/system files, `/` filters the current directory, `~` goes home,
  `q` quits, `Q` quits and writes the cwd.
- Typing a path: `:` opens a prompt that accepts any path form from rule 3, with Tab completion
  (completion also runs in the worker).
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
| `file` | ShellExecute "open" (Shift+Enter: reveal in explorer) | a spec `.xlsx` |
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
group = "AVEVA"

[[pin]]
kind    = "web"
name    = "grafana"
url     = "https://grafana.mycorp.com"
browser = "edge"       # edge | chrome | brave | <path to exe>; default edge (always on Win11)
```

- **Pin from anywhere:**
  - `P` pins the item under the cursor (folder → `folder`, `.exe`/`.bat`/`.lnk` → `app`,
    other file → `file`).
  - `Ctrl+P` pins the current directory.
  - From the launcher, "+ pin URL…" asks for a URL and a browser.
  - Each starts with a name prompt; group and key are optional.
- `'` + key jumps or launches directly. `'` + `'` opens the pin picker.
- In any picker, `Ctrl+E` edits, `Ctrl+D` deletes, `Ctrl+G` sets the group.
- Editing happens **in the app**. The user should never have to edit TOML by hand
  (though hand edits must still load).
- Opening an unreachable `folder` pin shows a status message and stays put. It never hangs.

### 4. Copy / move jobs

- `y` yank (copy), `x` cut, `p` paste into the current dir. Visual selection with `v`/`V`
  and `Space` to toggle.
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

- `r` rename (inline prompt), `a` new file, `A` new directory.
- `d` deletes to the Recycle Bin (`trash` crate) with a confirmation. `D` permanently deletes
  with a stronger confirmation. Network paths have no Recycle Bin: say so in the confirmation.
- All operations run in the worker and report back. Errors go to the status bar.

### 6. Launcher (ported from wtm)

- **Open with** `Ctrl+Space` (configurable). Avoid `Alt+Space`: Windows uses it for the
  window menu.
- The launcher is the picker over **all pins** (every kind), plus Start Menu apps on request.
- **Import from wtm:** on first run, if `%APPDATA%\wtm\config.toml` exists, offer to import
  its `[[launcher]]` entries (`name, command, args, dir, group, key`). A command
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
  prefix > group prefix > substring > fuzzy. Within a tier, sort by frecency, then
  alphabetically. With an empty query, show groups with headers and daily drivers on top.
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
| `Ctrl+Space` | Launcher | all pins + frecency |
| `''` | Pins | pins only |
| `Ctrl+F` / `sf` | Files | `WalkParallel` from cwd |
| `sd` | Dirs | frecency store (zoxide-like) + subdirs of cwd |
| `sg` | Grep (v0.2) | ripgrep crates, from cwd |
| `:` | Commands | every action, with its key shown (doubles as help) |

- The Files picker on a **network path** walks with a depth limit (default 4) and shows
  `network walk, depth 4`. `Ctrl+L` lifts the limit. Respect `.ignore` files. Skip
  `$RECYCLE.BIN` and `System Volume Information`.
- Picker actions: `Enter` open, `Ctrl+V` reveal in explorer, `Ctrl+Y` copy path, `Ctrl+P` pin,
  `Tab` multi-select (yank them into a copy job).

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
group   = "AVEVA"
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
  Then `Enter` runs it, `Esc` cancels. `Shift+Enter` skips the preview for `update` mode
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
group     = "AVEVA"
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
  `sentier pin <path|url> [--name N] [--group G]`, `sentier pins --json`,
  `sentier open <pin name>`. These edit `pins.toml` safely (atomic write + reload
  notification), even while a TUI instance is running.

## Config

`%APPDATA%\sentier\config.toml`, every field optional with defaults:
`show_hidden`, `timeouts.local_ms`, `timeouts.network_ms`, `jobs.robocopy_threads`,
`jobs.native_max_bytes`, `aveva.project_markers`, `keys.*` overrides, `launcher.open_key`.

## Milestones

1. **M1, skeleton:** terminal setup/teardown + panic hook, event loop, Miller columns over
   local disks through the async worker. `path.rs` with full tests.
2. **M2, network:** UNC browsing, the server node, Locations root, timeouts and an
   "unreachable" state. Test against a dead host (`\\10.255.255.1\x`) and confirm the UI stays
   responsive.
3. **M3, pins + picker:** `pins.toml`, the generic nucleo picker, pin picker, `'` jumps,
   in-app edits, reachability dots.
4. **M4, jobs:** robocopy backend + parser + fixtures, job panel, Finishing phase,
   reports, cancel/retry, AVEVA guard. Native backend.
4b. **M4b, project presets:** sync preset with `/L` preview + mirror safeguards, backup
   pipeline (staging → zip → verify → rename → retention), in-use check, last-run state,
   `sentier run`.
5. **M5, launcher:** app/web/file pins, wtm import, frecency, Start Menu scan, context
   actions.
6. **M6, pickers + polish:** files/dirs/commands pickers, path utilities, file ops,
   cwd-file wrappers, CLI, config overrides. Grep picker if time allows.

Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` only after the panic
hook has been verified to restore the terminal.

## Definition of done (every milestone)

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` pass.
- Manual check from a real WezTerm + PowerShell session on Windows 11, including one
  real SMB share and one unreachable share.
- No new `unwrap()` outside tests. No blocking call reachable from `app.rs` / `ui/`.

## Working agreements for Claude Code

- Build milestone by milestone. Don't scaffold empty modules for future milestones.
- When behaviour of a Windows API or robocopy is uncertain, write a tiny probe binary in
  `examples/` and ask the user to run it on Windows, rather than guessing.
- Ask before adding a crate not in the stack table.
