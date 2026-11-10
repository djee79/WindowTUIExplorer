# sentier

A keyboard-driven terminal file explorer for **Windows**, built for people who live on
network shares. Vim keys, Miller columns, and a copy engine you can trust with
many-gigabyte project folders.

> *Sentier* is French for trail, or path.

sentier runs in any modern terminal (WezTerm and Windows Terminal are the ones it's
tested in) and needs nothing else installed: robocopy, PDF rendering and thumbnails all
come with Windows.

**Status:** v0.1, under active development. Every milestone in the plan is code
complete; manual checks on real shares are still in progress. Windows only.

## Why

General-purpose terminal explorers fall short on the things a Windows admin does all
day:

- **Network shares are first-class.** `\\server\share\…` paths are bookmarked, browsed
  and jumped to like local folders. Every file system call runs off the UI thread with a
  timeout, so a dead share shows `⚠ unreachable` instead of freezing the app.
- **Copies you can trust.** Large copies go through robocopy and show what's actually
  happening: the phase, files and bytes done, throughput, ETA, locked files being
  retried, and a final report. No more "stuck at 99 %, no idea why".
- **One launcher for everything.** Folders, files, apps, web apps (opened as chromeless
  app windows) and saved copy jobs are all *pins* in one store, found with one fuzzy
  search.
- **Fast.** Rust, multi-threaded, with Telescope-style pickers: results stream in while
  you type, and only visible rows are drawn.

## Features

- **Navigation:** Miller columns (parent · current · preview), vim motions with counts
  (`5j`, `3dd`), relative line numbers, `/` search, `f` filter, history (`H` / `L`),
  tabs, sorting, and any path form: `\\server\share`, `C:\`, `%APPDATA%`, `$env:TEMP`, `~`.
- **Locations and shares:** drives (mapped drives show their UNC target), saved shares,
  a server's share list (`\\server`), reachability dots.
- **Pins and tags:** `m{key}` pins a folder like a vim mark, `'{key}` jumps there.
  Filter by tag in any picker (`#aveva`, `#a,b` for either, `!#old` to exclude),
  `is:unc`, `on:srv01`, and saved views.
- **Launcher** (`Space Space`): pinned apps, web apps, folders and files, ranked by
  frecency, plus Start Menu apps. It can import an existing
  [wtm](https://github.com/djee79/windowstilingmanager) launcher config.
- **Search** (`s` names, `S` contents): recursive, built on ripgrep's walker and
  matcher. Results stream into a folder you can browse, select, copy and preview.
  Supports fuzzy, glob and regex queries with `ext:`, `size:`, `mtime:` and `type:`
  filters.
- **Copy and move jobs:** robocopy with `/J /MT:8` (multi-threaded, unbuffered), a jobs
  panel with a throughput chart, pause and resume, cancel, and retries of locked files
  that you can skip or force. A final report keeps the log.
- **Sync and backup presets:** refresh a local copy of a server folder (update or
  mirror, always previewed with a dry run first), or take dated, verified zip backups
  with retention. They also run headless from Task Scheduler (`sentier run`).
- **Previews:** text and code, images (kitty, sixel or iTerm2 graphics, with a
  halfblock fallback), PDF pages, and anything Windows can thumbnail (DWG, Office,
  video…).
- **Open with** (`o` / `O`): rule-based openers (Neovim inline, GUI apps detached) plus
  the apps Windows suggests for the file type.
- **Bulk rename:** PowerRename-compatible syntax (regex, `${}` counters, date and EXIF
  tokens), a live preview with collision checks, and real undo.
- **Zip and extract:** `.zip` built in, everything else through 7-Zip when installed.
- **Machines:** open `\\PC\C$` on users' machines through path templates ("their
  Desktop", "their temp folder"). Admin shares turn the header red, confirm every
  change, and write an audit log.
- **Credentials:** when Windows refuses access, a masked prompt in the terminal
  connects the share. It can save the password to Windows Credential Manager (off by
  default) and fixes error 1219 (already connected as someone else).
- **Discoverable:** a which-key popup for every prefix, `g?` for the full keymap, and
  `Space :` to search every command.

## Install

Requirements: Windows 10 or 11 and a recent Rust toolchain (stable, with edition 2024
and let chains: 1.88 or newer).

```powershell
git clone https://github.com/djee79/WindowTUIExplorer.git
cd WindowTUIExplorer
cargo install --path .
```

That puts `sentier.exe` in `%USERPROFILE%\.cargo\bin`. Optional:
[7-Zip](https://www.7-zip.org/) for `.7z` / `.rar` / `.tar` archives and a faster backup
engine.

### Shell integration

To have your shell follow sentier's current folder when you quit, load the PowerShell
wrapper in your `$PROFILE`:

```powershell
. C:\path\to\WindowTUIExplorer\shell\sentier-shell.ps1
```

Then start sentier with `s`. Quit with `ZZ` (or `Q`) to land in the folder you were
looking at; `q` quits and leaves the shell where it was.

## Getting started

```powershell
sentier                      # start in the current folder
sentier \\srv01\projects     # or anywhere else
```

| Keys | |
|---|---|
| `h` `j` `k` `l` | parent · down · up · open |
| `gl` | Locations: pins, drives, shares |
| `Space Space` | launcher |
| `s` / `S` | search names / contents |
| `Tab` `v` `V` | select · visual mode |
| `yy` `x` `p` | copy · cut · paste (starts a job) |
| `dd` / `D` | to the Recycle Bin / delete permanently (both ask first) |
| `r` | rename (with a selection: bulk rename) |
| `m{key}` / `'{key}` | pin this folder / jump to a pin |
| `J` | jobs panel |
| `:` | command line (`:cd`, `:sort`, `:sync`, `:tags`…) |
| `g?` | every key, searchable |
| `q` | quit |

Pause after `Space`, `g`, `y`, `d`, `z` or `'` and a popup lists what comes next.

## Configuration

Everything works without configuration. Optional files:

| File | What |
|---|---|
| `%APPDATA%\sentier\config.toml` | settings: hidden files, line numbers, timeouts, preview protocol, editor, `[jobs] robocopy_threads` / `keep_logs_days`, auth profiles, machine path templates |
| `%APPDATA%\sentier\pins.toml` | pins, tags, saved views, sync and backup presets (managed from the app; hand edits load too) |
| `%APPDATA%\sentier\keymap.toml` | key remaps (vim notation, prepend / append per mode) |
| `%APPDATA%\sentier\open.toml` | openers and the rules for `o` / `O` |

A bad setting is reported in the status bar, never a crash. The full reference is the
spec, [CLAUDE.md](CLAUDE.md).

### Files sentier writes

Under `%LOCALAPPDATA%\sentier\`: logs (`logs\`, rotated daily, 7 kept), job logs and
reports (`jobs\`), the rename journal, frecency (`usage.tsv`) and UI state. It cleans up
after itself: job logs older than 30 days go at startup (unless their report is still
listed), backup staging leftovers are removed, and `audit.log` (changes made on admin
shares) is the only file never trimmed.

Set `SENTIER_LOG=debug` for verbose logging.

## Command line

```text
sentier [path]                         start there
sentier --cwd-file <file>              write the final folder there (used by the wrapper)
sentier run "<preset>" [--allow-mirror] [--threads n]
                                       run a sync / backup preset without the UI
sentier pin <path|url> [--name N] [--tag T]... [--key K]
sentier pins [--query "#tag is:unc"] [--json]
sentier open "<pin name>"
```

`sentier run` returns robocopy-style exit codes (0–7 ok, 8 failed, 16 couldn't run), so
Task Scheduler can run nightly backups.

## Development

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- [CLAUDE.md](CLAUDE.md) is the full specification: features, keymap, architecture
  rules (no blocking I/O on the UI thread, timeouts on every file system call).
- [docs/STATUS.md](docs/STATUS.md) is the build log: what's done, what was measured,
  and what's pending.
- `examples/` holds probes for Windows behaviour, e.g.
  `cargo run --example capture_robocopy` recaptures the robocopy output fixtures the
  parser is tested against.
