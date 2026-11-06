# Where we are: handoff, 2026-11-05

Written when moving from the Linux dev box to Windows. Read this alongside
`CLAUDE.md` (the spec) and `docs/screens/*.txt` + `docs/theme.toml` (the visual design).

## Milestone status

| Milestone | State |
|---|---|
| **M1 skeleton** | Code complete. Passes fmt, clippy and tests on Linux, and clippy for the Windows target. **Never run on Windows yet.** |
| M2 network | Not started (path model for it is done, see below) |
| M3–M6 | Not started |

## What exists

```
src/main.rs            args (--cwd-file, start path, -h, -V), logging, panic hook,
                       terminal setup/teardown, tokio runtime, input thread
src/app.rs             App state, event loop (dirty redraw, ~60 fps cap, spinner tick only
                       while busy), key dispatch, + tests (headless render, keys)
src/event.rs           AppEvent (Term, Listed, ListProgress, DiskSpace, Opened, InputClosed)
src/fs/path.rs         Loc = Root | Server(\\srv) | Fs(drive or share path); parse/resolve/
                       parent/join/display/os_string (\\?\ prefix above 247 chars); tests
src/fs/worker.rs       listing worker: batched read_dir on spawn_blocking + async supervisor
                       with a NO-PROGRESS timeout (500 ms local / 3 s network, mapped drives
                       count as network), cancellation, disk free space, ShellExecute open
src/fs/mod.rs          Entry, Listing, natural sort (dirs first); tests
src/platform/windows.rs  GetLogicalDrives, GetDriveTypeW, GetDiskFreeSpaceExW, ShellExecuteW,
                       file attributes
src/platform/shim.rs   non-Windows stub so `cargo test` builds on Linux (approved; not a port)
src/ui/                theme.rs (colors from docs/theme.toml, compiled in), columns.rs
                       (3 rounded panels per 01-main), statusbar.rs (top bar, status line,
                       key hints), help.rs (? overlay)
shell/sentier.ps1      PowerShell `s` wrapper (Q writes cwd)
```

Keys working: `hjkl`/arrows, `gg` `G` Home End, `^d` `^u` PgUp PgDn, Enter/`l` (dir → enter,
file → ShellExecute open), `h` (C:\ → Locations root), `gl`, `~`, `.`, `/` filter
(Enter keep, Esc clear), `R` retry/refresh, `?`, `q`, `Q`, `^C`.

Behaviour worth knowing:
- Cached listings show instantly and refresh in the background. Going up re-selects the dir
  you came from; each dir remembers its cursor.
- The preview is debounced by 40 ms. Requests for dirs no longer on screen are cancelled.
- A refresh that fails over a stale listing shows the error (the place is unreachable now).
- On exit the runtime uses `shutdown_background()`, so readers stuck on a dead share don't
  block quitting.
- The Locations root lists **drives only** in M1. `\\server` listing returns
  "not supported yet" (NetShareEnum is M2).

## Decisions taken (user-approved)

- Added crate `tracing-subscriber` (env-filter). Log level via env `SENTIER_LOG` (default info).
- `windows` crate 0.62 is a `cfg(windows)` target dependency. The dev shim is under
  `cfg(not(windows))`.
- Long-path threshold is 247 UTF-16 units (stricter than the spec's 260, to cover the
  CreateDirectoryW 248 limit).
- Sizes use 1024-based KB/MB/GB with one decimal ("612.0 MB"), like the mocks.
- The release profile has `lto = "fat"` and `codegen-units = 1`. `panic = "abort"` is NOT set
  until the panic hook is verified on Windows.

## First thing to do on Windows

```powershell
cargo fmt --check; cargo clippy --all-targets -- -D warnings; cargo test
cargo run                                   # starts in the current dir
cargo run -- \\<real-server>\<share>        # real SMB share
cargo run -- \\10.255.255.1\x               # dead host: expect "⚠ unreachable (timed out)"
                                            # after ~3 s, UI responsive throughout, R retries
```

Manual checklist for M1 "done":
- [ ] Panels and colors look like `docs/screens/01-main.txt` in WezTerm.
- [ ] Keys respond once per press (crossterm on Windows also sends Release events; we filter
      to Press/Repeat. Verify there's no double-move).
- [ ] `h` from `C:\` shows drives, and Enter on a drive enters it.
- [ ] Mapped network drive: listing works and gets the 3 s timeout.
- [ ] Hidden/system files are hidden by default and `.` shows them (dimmed).
- [ ] Enter on a file opens it in its associated app.
- [ ] Free space shows in the status line (local and share).
- [ ] Junctions (e.g. `C:\Users\<you>\Application Data`) show as `name\ →` and don't stall.
- [ ] A folder with 50k+ files: loading counter climbs, no timeout, scrolling is smooth.
- [ ] Panic hook: temporarily add a `panic!()` on some key and confirm the terminal is
      restored. Then `panic = "abort"` may go into the release profile.
- [ ] `. shell\sentier.ps1`, run `s`, press `Q`: the shell lands in sentier's directory.
- [ ] Logs appear in `%LOCALAPPDATA%\sentier\logs\`.

## Open questions about the mockups (settle before M2–M5)

1. `01-main` top bar shows `tab 1 of 2`. Tabs aren't in CLAUDE.md. Add them to the spec or drop them.
2. `02-locations` shows `e rename`, `dm delete pin`, `P pin current dir`. The spec says
   `^E`/`^D` in pickers, `P` = pin item under cursor, and `^P` = pin current dir.
3. `02-locations` counts don't add up: 5 pinned folders are listed, the footer says 4 and
   "16 locations", and the header says "3 of 4 pins reachable".
4. `04-jobs` shows a `copy · native` job whose report says `robocopy exit 9`. Native jobs need
   their own result wording.
5. `~\Downloads` display needs a home-abbreviation rule in `path.rs`. The top bar's `12 ms`
   needs a defined measurement (probe RTT?). M1 shows only the dot and the server name.

## M2 (network): code complete 2026-11-06, manual check pending

- `examples/probe_netapi.rs` measured it: `NetShareEnum` on a dead host parks the thread
  for **21 s** (error 53). Every network call therefore runs on the blocking pool under a
  timeout the supervisor gives up on; late results are dropped.
- `platform`: `shares()` (NetShareEnum level 1, admin/IPC/temporary shares filtered),
  `mapped_target()` (WNetGetConnectionW; works for disconnected mappings), `volume_label()`
  (local disks only: on a mapped drive it would go to the server).
- Server node `\\srv` lists its shares (3 s timeout; access denied says to type the share).
- Locations root is its own screen (`ui/locations.rs`, per 02-locations): DRIVES (label, or
  `→ \\srv\share` + `mapped` / `disconnected`) and SHARES sections, dots, detail panel,
  legend. PINNED FOLDERS joins in M3.
- Reachability: `Worker::probe` = `metadata()` + free space, 2 s timeout. Runs when the root
  opens, for new entries, every 60 s while it's on screen, and on `R`.
- Opening an entry whose last probe failed stays put, says why, and re-probes.
- Saved shares: `%APPDATA%\sentier\shares.txt`, one `\\server\share` per line (plain text,
  no new crate, atomic write). A share is added when a listing under it first succeeds;
  `dd` on a share in Locations removes it. Shares browsed before the file loads are merged.

Manual checklist for M2:
- [ ] `cargo run -- \\10.255.255.1\x`: `⚠ unreachable (timed out)` after ~3 s; keys keep
      working throughout; `R` retries; `q` quits at once.
- [ ] Browse a real share, `h` → its server lists shares, `h` → Locations shows it saved.
- [ ] Mapped drives show `→ \\srv\share`; a disconnected one shows `× … disconnected`.
- [ ] Add `\\10.255.255.1\x` to shares.txt by hand: its dot turns × after ~2 s, Enter
      stays put, the UI never stalls.

## M2a search: `s` name search and `S` content search done (2026-11-06)

- `src/search/query.rs`: fuzzy / glob (`* ? [`) / `re:` + `ext: type: size: mtime: depth:
  hidden: follow: !segment`, with table tests.
- `src/search/name.rs`: nucleo fuzzy over the relative path + a bonus when the name
  matches too; globset (case-insensitive; path glob when it has `\`); smart-case regex.
- `src/search/walk.rs`: own thread pool over `FindFirstFileExW` (16 threads on UNC, cores
  locally), matching in the walker threads, ~16 ms batches + 150 ms progress, pruning
  ($RECYCLE.BIN, System Volume Information, hidden/system, `!seg`, no link following
  unless `follow:yes`, capped at depth 64), `.ignore` files with `!` re-includes.
- `Loc::Results { id, origin }`: results are a folder in the history; its parent is the
  origin; names are relative paths resolved against the origin (can't escape it).
- `src/app/search.rs`: up to 8 searches kept, dedup across threads, re-sort throttled to
  50 ms, 200k hit cap, sort modes (`,s`), `Esc` = cancel walk / then back, `gr`, `s` again
  = prefilled refine (old hits re-scored off-thread while a fresh walk runs).
- UI: search header with live `scanned · found · s` + spinner, `⚠ no answer for N s` after
  3 s without progress, Path/Size/Score columns, "walking …" in the status line,
  "Found in … gr" in the preview.

Pending:
- `examples/enum_bench.rs`, first result (2026-11-05, home Synology over LAN,
  `\\homenas\Media\videos`: 1 714 dirs, 2 280 files, ~1.3 files/dir):

  | threads | A WalkParallel | B read_dir | C FindFirstFileExW large |
  |---|---|---|---|
  | 1  | 17.31 s | 17.41 s | 17.29 s |
  | 16 | 1.47 s  | 1.43 s  | 1.37 s  |
  | 32 | 1.41 s  | 1.36 s  | 1.23 s  |

  Same counts everywhere. ~10 ms per directory open: latency-bound, so threads give 12×.

  Second result, big directories (`\\homenas\photos\originals`: 286 dirs, 32 295
  files, ~113 files/dir), warm runs: A 1.75 s / B 1.19 s / **C 1.08 s** at 16 threads;
  at 32 threads A 1.60 / B 1.23 / C 1.34 (the NAS gets slower past 16).

  **Decision: search uses its own walker on `FindFirstFileExW` large fetch, 16 threads
  on UNC** (`platform::find_entries`, `search/walk.rs`). `.ignore` is read only when the
  listing shows one (WalkParallel probed every directory). Cancel no longer waits for a
  thread stuck in SMB. The `ignore` crate stays for its gitignore matcher. Still worth
  a run on the office file server (`\\srv01\project\HIT`).
- `S` content search: done and manually checked. `src/search/content.rs` (ripgrep crates:
  literal smart-case or `re:`, `multi_line` + `crlf` so `$` works on CRLF files, binary
  skipped, UTF-16 BOM transcoded, name filters still apply), match counts per result,
  `src/ui/grep.rs` preview with context lines, `]m`/`[m` step through matches.
- Optional per-share index (`:index add`) is a stretch goal.

## Spec update adopted 2026-11-06 + keymap pass (code complete, manual check pending)

CLAUDE.md and docs/screens now come from Claude Design (the copy in docs/sentier-screens/
can be deleted). New since M1: vim-first keymap, pin tags (M3), previews (new M2b), Yazi-style
`s`/`S` search (new M2a), relative line numbers (M1). The non-Windows `platform/shim.rs`
is gone ("no cfg-gated Linux code").

Keymap pass:
- `src/keymap.rs`: vim notation (`<C-d>`, `<leader>`, `<S-CR>`), actions, the default
  normal-mode table with descs, and `%APPDATA%\sentier\keymap.toml` (`leader`,
  `timeout_ms`, `[normal] prepend/append/keymap`; problems go to the status bar + log;
  tables for later modes such as `[picker.insert]` are ignored). Loaded by the worker under
  the network timeout (APPDATA may be redirected to a share).
- `src/app/keys.rs`: sequence dispatch with counts, which-key after 300 ms (`Esc` cancels,
  Backspace backs up a level), all actions, prompts, search, history, completion.
- `src/prompt.rs`: the line editor (readline basics in insert, `h l w b e 0 $ x i a I A dw
  cw ciw C D u` in normal).
- Keys now: `hjkl` + counts, `gg G {n}G :{n}`, `^d ^u ^f ^b`, `zz zt zb`, `/ ? n N`
  (smartcase, wrapscan, hlsearch until `Esc`), `f` filter, `H L ^o` history, `''`, `-`,
  `~ gl gd g\`, `g.`, `g?`, `<leader>l`, `R`, `dd` (Locations only), `:`, `q ZZ ZQ`
  (`Q` kept as a ZZ alias because spec section 10 still says Q).
- `:` commands: `cd`/`e <path>` with Tab/S-Tab completion from the listing cache (`:e` on a
  file shows it selected in its folder), `q q! wq x`, `{n}`, `set nu|nonu|rnu|nornu`.
  Future ones (`pin`, `sync`, `rename`…) say which milestone brings them.
- Hybrid line-number gutter in the current column (`:set` toggles; `numbers` in
  config.toml waits for config.rs).
- Keyboard enhancement (kitty protocol) is pushed when crossterm says it's supported.
  `examples/probe_keys.rs` answers the spec's "verify under WezTerm/ConPTY" question.

`o` is bound to the same open as `l`/Enter until open.toml arrives (M5 is now "launcher +
open-with": rules/openers, the `O` menu, `gR`, SHAssocEnumHandlers, inline Neovim).

Not yet (their milestones): `O` / `gR` open-with (M5), `m`/`M`/`'{key}` pins, `Tab v V` selection, `yy x p P`, `y*`
path copies, `]p [p zp`, pickers and `<leader>` entries other than `l`,
counts on operators.
