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

## M3 pins, slice 1: store, marks, Locations (code complete 2026-11-06, manual check pending)

- `src/pins.rs`: `%APPDATA%\sentier\pins.toml`, all six kinds. Lenient load: a broken
  `[[pin]]` is reported and written back unchanged (never dropped); unknown fields,
  `[[view]]` and `[tags.*]` round-trip; bad tags/keys are dropped with a message.
  Comments in a hand-edited file are lost on save (the `toml` crate drops them).
  Atomic save through one writer task (newest store wins). Nothing is saved until the
  file has been read, so a failed read never gets overwritten.
- Keys: `m{key}` pins the current folder (or just moves the key if the folder is
  already pinned, like a vim mark), `'{key}` jumps (folder) / opens (file), `''` back,
  `M` pins the item under the cursor (dir → folder, .exe/.bat/.cmd/.lnk → app, else
  file). Prompts: name → tags (Tab completes, most used first) → key. `Esc` pins nothing.
- Keymap notation `<char>`: any plain character (`m<char>`, `'<char>`); an exact binding
  (`''`) beats it. which-key after `'` lists every pin with its key; after `m`, the
  keys already taken.
- Locations: PINNED FOLDERS section first (dot, `'a`, name, path), probed like shares;
  a known-unreachable pin stays put and re-probes. Detail panel shows key + tags.
  On a pin: `r` rename, `T` tags, `dd` unpin (asks, `y` confirms).
- App/web/sync/backup pins load and keep their `'` key, but run in M5/M4b.

Manual checklist:
- [ ] `g<Space>\homenas\Media<CR>`, `mm`, name + tags, then `gl`: pin listed with ●.
- [ ] `'m` from anywhere jumps there; `''` comes back.
- [ ] `'` and wait: which-key lists the pins.
- [ ] Quit and restart: pins are still there (`%APPDATA%\sentier\pins.toml`).
- [ ] Hand-edit pins.toml with a typo'd pin: startup says so, the pin survives a save.
- [ ] Pin `\10.255.255.1\x` (edit the file): × after ~2 s, `'key` stays put.

|delete`, saved views.

## M3 pins, slice 2: generic picker + pin picker (code complete 2026-11-06, manual check pending)

- `src/picker/query.rs`: the pin query. `#tag` (prefix), `#a #b` AND, `#a,b` OR, `!#tag`,
  `is:folder|file|app|web|sync|backup|unc|local|unreachable` (and `!is:`), `on:<server>`
  (prefix; mapped-drive pins resolve through the Locations listing), rest = fuzzy text.
  Completion of the word being typed with counts. Table tests.
- `src/picker/mod.rs`: generic picker on `nucleo::Nucleo` (own thread pool, wakes the UI
  through `AppEvent::Wake`). Filter words refill the matcher; text reparses the pattern.
  Ranking: wtm tiers (name prefix › tag prefix › substring › fuzzy), stable over nucleo's
  score order (top 5 000). Tags are part of the matched text. Tag bar, `]t`/`[t` cycling,
  match highlighting for visible rows only.
- `<leader>p` opens it (insert mode). `^j/^k ^n/^p` move, `Enter` opens (folder → go,
  file → ShellExecute), `Tab` completes a filter word, `^e` edit (name → tags → key),
  `^t` tags, `^x` delete (y/n); edits return to the picker with the same query.
  `Esc` → normal mode: `j/k` + counts, `gg/G`, `^d/^u`, `]t/[t`, `i/a`, `q/Esc` close.
  Relative-number gutter in normal mode. Preview: pin details, reachability, and a folder
  pin's listing (same cache and debounce as the columns; skipped when unreachable).

Not yet: `^y` copy path / `^v` reveal (with path utilities, M6 / open-with, M5), Tab
multi-select (needs jobs, M4), the query syntax in the Locations filter `f`, `:tags`,
`:tag rename|delete`, `<leader>t` tag picker, saved views (`:view save`).

Manual checklist:
- [ ] `<Space>p` with a few pins: list, counter `n / N`, tag bar.
- [ ] Type `#av` + Tab, `is:unc`, `on:homenas`: the list narrows instantly.
- [ ] `Esc`, `5j`, `gg`, `]t` `]t` `[t`, `i`, `q`.
- [ ] `^t` / `^e` / `^x` on a pin: back in the picker afterwards with the query kept.
- [ ] Preview of a folder pin on the NAS lists its contents; a dead one says so, no stall.

## M3 pins, slice 3: views, tag management, Locations query (code complete 2026-11-06)

M3 is code complete with this slice; the manual checks below are pending.

- Saved views (`[[view]]` in pins.toml: name, query, optional key; lenient load like pins,
  broken ones kept). `:view save <name>` saves the pin picker's last query (the query it
  had when it closed), then asks for a key; `:view delete <name>`. Views and pins share
  the `'` key space (a key moves like a vim mark). `'v` on a view opens the pin picker
  with its query. In the pin picker views are `[view]` rows (`is:view`): Enter applies,
  `^e` rename + key, `^x` delete.
- Tags: `:tags` / `<leader>t` open the tag picker (counts, most used first; preview lists
  the pins). `Tab` chooses, `Enter` opens the pin picker with `#a #b` (all required; type
  `#a,b` in the pin picker for "either"). `^e` renames, `^x` deletes.
  `:tag rename <old> <new>` / `:tag delete <tag>`: y/n first, saying how many pins change
  (and when a rename merges into an existing tag). `[tags.<name>]` settings move along.
- `:pin` pins the current folder (or edits its pin).
- Locations `f` takes the pin query: `#tag`, `is:unc|local|unreachable|folder`, `on:`,
  plus words as substrings of the name/detail.
- Picker rows show the reachability dot (● × ○); the tag picker shows ✓ for chosen.
- y/n questions share one `Confirm` prompt (app/tags.rs).

Left for later milestones (by design): empty-query grouping by first tag and frecency
(launcher, M5), `^y` / `^v` (M6 / M5), Tab multi-select in the pin picker (M4 jobs),
`[picker.insert]` keymap tables (with config, M6), per-tag colors on the chips.

Manual checklist:
- [ ] `<Space>p`, type `#aveva is:unc`, `Esc q`, `:view save aveva shares`, key `v`;
      then `'v` from anywhere; `'` + wait lists the view.
- [ ] `<Space>t`: Tab two tags, Enter: pin picker shows pins with both.
- [ ] `:tag rename x y` says how many pins, `y` applies; pins.toml shows it.
- [ ] `gl`, `f#nas<CR>`, `f is:unc`: Locations narrows; `Esc` clears.

## Spec update adopted 2026-11-06 (second Claude Design drop)

New: section 14 Machines (admin shares on user PCs: machine pins, 445 probe, machine view
with path templates, logged-on user, admin-share safety + audit log), section 15
Credentials (masked prompt, WNetAddConnection2W, Credential Manager, error 1219 fixes,
`:net`), DFS resolution (rule 3), crate `zeroize`, `is:machine|online|offline`, `gm`,
`<leader>m`, screens `10-machine` and `11-credentials`, milestones M2c (credentials) and
M3b (machines). The module layout was fixed on adoption: `machines/` and `auth/` are
top-level, and `zip.rs` / `inuse.rs` / `report.rs` live under `jobs/`.
Until M3b, a `kind = "machine"` pin loads as a reported, preserved broken pin.

Order from here: M2b previews → M2c credentials → M3b machines → M4 jobs.

## M2b previews, slice 1: image previews (code complete 2026-11-06, manual check pending)

Probe results (`examples/image_probe.rs`, WezTerm 20240203 on Windows 11):
- The terminal query never answers through ConPTY (fails in 0 ms), so auto-detection is
  useless: the protocol comes from `TERM_PROGRAM` (WezTerm → iTerm2, anything else →
  halfblocks; `SENTIER_PREVIEW_PROTOCOL` forces one until config.toml exists) and the cell
  size is 10×20 px (`preview.cell_px` later; the card's shape looked right at 10×20).
- iTerm2: shows. Sixel, kitty: nothing. Halfblocks: shows.
- An iTerm2 picture stays on screen until its cells are written again. ratatui never
  rewrites them (ratatui-image marks them "skip"), and a full clear doesn't remove it
  either. Writing spaces over the cells does, and so does text drawn there.
  → After each frame, if the picture moved, changed size or went away (or the window was
  resized), the UI loop re-emits the old picture's cells from the frame just drawn
  (`preview::repaint_cells` + `Backend::draw`). No flicker, no full clears.
- Windows Terminal not probed yet (gets halfblocks for now).

Code:
- `src/preview/`: `PreviewKind` (png jpg gif webp bmp tiff ico), protocol choice, an
  in-memory LRU (64), `repaint_cells`, the 50 MB network guard; `image::load` decodes and
  encodes on the blocking pool (the UI only renders an encoded picture).
- `Worker::image_preview`: 5 s local / 15 s network; network files over 50 MB (mapped
  drives included) say so instead of reading. Late answers are dropped by key.
- App: asked after 60 ms of cursor stillness; the box comes from the UI's last frame, so a
  resize refits it; cache hits are instant. Caption: `400×200 · PNG · 4.0 KB`. No picture
  under the help overlay or a picker.

Next slices: EXIF for photos (`kamadak-exif`), PDF pages (WinRT `Windows.Data.Pdf`) with
`]p`/`[p`/`gP`, shell thumbnails (`IShellItemImageFactory`), `zp` full screen + zoom,
preload next/previous, the disk cache for network files, a Windows Terminal probe run.

Manual checklist:
- [ ] A folder of photos in WezTerm: pictures appear after the cursor stops; holding `j`
      doesn't stutter; moving to a .txt or a folder leaves no picture behind.
- [ ] Resize the window with a picture shown: it refits, no ghost.
- [ ] `g?` and `<Space>p` over a picture: no picture bleeding through.
- [ ] A big photo on the NAS: spinner, then the picture; one over 50 MB says so.

### M2b fix: iTerm2 pictures landed in the wrong place (2026-11-06)

Seen in sentier: the picture appeared bottom right, cut off, and stayed after moving on.
`examples/image_place.rs` found why: an iTerm2 picture lands wherever the real terminal's
cursor is, and with the cursor hidden (ratatui hides it) ConPTY never sends cursor moves.
Round 2 results: a picture inside ratatui's frame landed right only half the time (small
update and full frame alike); written after the frame with the cursor shown, moved, then
hidden again: both in place, with or without a 15 ms wait.
→ The UI marks where an iTerm2 picture goes; the UI loop writes it after the frame with
`preview::place_iterm2`, only when it changed (new picture, moved, resized). Halfblocks
still render through ratatui. While the which-key popup is up the picture is hidden too
(a picture written after the frame would cover the popup).

### M2b fix: iTerm2 pictures came out at half size in a black box (2026-11-06)

On a 200 % screen (5120×2880) a cell is ~21×43 px, not the assumed 10×20, and ratatui-image
sizes iTerm2 pictures in pixels (and blanks the box first), so pictures filled a quarter of
a black box. sentier now has its own iTerm2 encoder (`preview::image`): size given in
**cells** (`width=N;height=N;preserveAspectRatio=1`), so the terminal scales to the box at
any screen scaling; sent at 2× resolution for sharpness; no blanking. Pictures fill the box
keeping their shape; small ones (icons) keep their natural size. Base64 is a small local
function (no new crate). The first frame and every resize now rewrite every cell, which
also clears pictures left in the terminal by earlier programs.

### M2b fix: "timed out after 5 s" on big PNGs (2026-11-06)

The 3311×2851 cheatsheet timed out in a debug build: unoptimized `image` plus a filtered
resize took over 5 s, and the timeout covered decoding too. Now: dependencies are optimized
even in debug builds (`[profile.dev.package."*"] opt-level = 3`); the timeout covers reading
the file only (`preview::image::read`, the part that can hang on a dead share), decoding is
untimed; big reductions use `thumbnail`. Measured on the real files: ~20 ms decode, ~70 ms
decode + shrink + PNG for 3311×2851 and 5120×2880. Files over 256 MB aren't previewed.

## M2b previews, slice 2: PDF pages (code complete 2026-11-06, manual check pending)

- `platform::pdf_page`: WinRT `Windows.Data.Pdf` (features `Data_Pdf`, `Storage_Streams`,
  `Foundation`, `Win32_System_WinRT`). Loads from memory (the file read stays under the
  preview timeout), renders one page to PNG, returns page count and size in DIPs. The
  render width is in DIPs, so Windows adds the display scaling (≥ the width asked).
  Measured on real files: 35-140 ms a page, including a 201-page manual.
- The page goes through the same encoder as pictures (iTerm2 sized in cells / halfblocks),
  so placement, erasing, caching and HiDPI all carry over. `ImageKey` has the page.
- Caption: `page 2 / 14 · A3 landscape · 420×297 mm (16.5×11.7 in) · 1.2 MB`; sheet names
  A0-A4, ANSI A-E, Arch C-E (±3 mm, either way round). Spinner: `rendering page 2…`.
- Keys: `]p` / `[p` (counts: `5]p`), `{n}gP`, `gP` alone prompts for the number. Stops at
  the first / last page. Each PDF keeps its page while sentier runs.
- In search results, picture/PDF previews show `Found in …` above the caption.

Not in this slice: the PDF title (Windows.Data.Pdf doesn't expose it), preloading the next
page, `zp` full screen + zoom, password-protected PDFs (they show the error).

Manual checklist:
- [ ] A drawing PDF: page fills the column, caption names the sheet size.
- [ ] `]p` `[p` `10]p` `gP` on a long PDF; leave and come back: same page.
- [ ] A PDF on the NAS; one over 50 MB says so.

## M2b previews, slice 3: Windows thumbnails (code complete 2026-11-06, manual check pending)

- `platform::shell_thumbnail`: `SHCreateItemFromParsingName` → `IShellItemImageFactory::
  GetImage` (`SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK`: no icon fallback) → HBITMAP →
  `GetDIBits` (32-bit, top-down) → premultiplied BGRA to straight RGBA. Bitmap always
  freed. Feature `Win32_Graphics_Gdi`. Measured: ~200 ms for a PNG/JPEG thumbnail; a file
  without a handler fails in ~6 ms (Windows gives an empty message).
- `PreviewKind::Thumbnail`: every extension that isn't a picture / PDF and isn't on a skip
  list (text, code, PML, archives, programs, temp files). Office files, videos, HEIC, DWG
  (with Autodesk's handler), Revit… get one when a handler is installed.
- Same encoder, placement and cache as pictures. Timed as one call (Windows reads the file):
  3 s on the network, 5 s locally. Network files over 50 MB skipped.
- UI: the file info shows while Windows is asked and when there's no thumbnail (no spinner,
  no error); with one, the picture and `Windows thumbnail · 1.2 MB`.

Note: on this machine PDFs have no thumbnail handler; they use Windows.Data.Pdf anyway.

Manual checklist:
- [ ] A .docx / .xlsx / .mp4: thumbnail or file info, never a spinner flash.
- [ ] At the office: a .dwg with Autodesk's thumbnail handler installed.

## M2b previews, slice 4: `zp` full screen + zoom, preloading (code complete 2026-11-06)

- `zp` (on a picture, PDF or thumbnail-type file): the preview fills the body (`Mode::Preview`).
  `j`/`k` next / previous file (zoom resets), `]p`/`[p` pages (counts), `+`/`-` zoom
  100 · 150 · 200 · 300 · 400 · 600 · 800 %, `=` back to 100 %, `H J K L` pan a quarter
  of the visible part, `zp`/`Esc`/`q` back. Status bar: `PREVIEW · zoom 200 %`; title shows
  the zoom. These keys are fixed for now (keymap tables for other modes come with config).
- Zoom is a real crop (`preview::crop_rect`, `View` in the `ImageKey`): the visible part is
  cut out and fitted to the box, growing past its natural size. PDFs are re-rendered wider
  when zoomed (up to 12 000 DIPs) so text stays sharp; thumbnails are asked bigger.
- Preloading: once the current preview is shown, the nearest previewable file on each side
  and the next PDF page are requested into the cache (not while zoomed). A request already
  on its way is never sent twice; landing on a file being preloaded waits for that answer.
- "No thumbnail" (and other non-timeout failures) are remembered (LRU of 256), so moving
  back and forth doesn't ask the shell again.

Manual checklist:
- [ ] `zp` on a photo, `+ + +`, `H J K L`, `=`, `j` to the next photo, `zp` back: no ghost.
- [ ] `zp` on a drawing PDF, `++`, pan to the title block: text sharp. `]p` pages.
- [ ] In a folder of photos, `j` after the first preview: the next one appears instantly.

## M2b previews, slice 5: photo EXIF + orientation (code complete 2026-11-06)

- Crate `kamadak-exif` 0.6.2 (in the stack table). `preview::image::photo_info` reads the
  EXIF of JPEG / TIFF / HEIF / PNG / WebP from the bytes already in memory: date taken
  (`DateTimeOriginal`, else `DateTime`), camera (make + model, without repeating the maker),
  GPS yes/no. Shown on the row above the caption:
  `taken 2024-06-01 14:32 · Apple iPhone 13 · GPS` (search results keep `Found in` there).
- Pictures are turned upright from their EXIF orientation (`image`'s decoder orientation +
  `apply_orientation`): portrait phone photos no longer show sideways. The caption's size
  is the upright one.

M2b is complete except the on-disk thumbnail cache for network paths (spec: 500 MB under
`%LOCALAPPDATA%\sentier\cache\thumbs\`), left for when previews over SMB prove slow.

## M2c credentials (code complete 2026-11-06, manual check pending)

Layout note: the spec puts WNet / Credential Manager under `auth/`, but rule "all Win32
calls live in src/platform/" wins: the calls are in `platform` (`net_connect`,
`net_disconnect`, `net_connections`, `cred_write`); `src/auth/` is pure (error messages,
targets, profiles, `Secret`); `src/app/auth.rs` the flow; `src/config.rs` reads
`[auth.<name>] user = '…'` from `%APPDATA%\sentier\config.toml` (other settings later).

- Listing errors: 1326 / 86 / 1244 / 1330 / 1331 / 1909 are now `ListError::LogonFailure`
  with plain words (`auth::error_message`), like 5 (access denied).
- When the cwd on a share is refused, a masked prompt opens (screen 11), once per server
  until a connection works; `:connect` opens it by hand. Target: the share root, or
  `\server\IPC$` for a server node. Admin shares (`X$`) prefill `[auth.admin]`.
  Tab / S-Tab fields, Space toggles Remember (off by default), Enter connects, Esc cancels.
  Password: `Zeroizing<Vec<char>>` in the prompt, `Secret` (wiped UTF-16) to the worker;
  never drawn (● only), logged or stored. Pasting works (Windows delivers pasted text as
  keys; crossterm has no bracketed paste there).
- `WNetAddConnection2W` (`CONNECT_TEMPORARY`, no letter), 25 s timeout on the blocking pool.
  Wrong password: asked again with the reason, 3 tries. Remember: `CredWriteW`
  (`CRED_TYPE_DOMAIN_PASSWORD`, local machine) for the server, only after success.
- Error 1219 keeps the password and offers `d` (drop every connection to the server, mapped
  drives included (back at next logon), then connect) or `a` (connect through the server's
  IPv4 address, then open the same folder on it).
- `:net list` (overlay: letter, remote, user, made-by-sentier), `:net drop <server>` (not
  drive letters; says which were left), `:net drop-all` (the ones sentier made).
- Pictures are hidden under the prompt and the list (they'd cover them).

Not yet: DFS target resolution (`NetDfsGetClientInfo`, rule 3), the "*.domain" wildcard
option for Remember, `auth = "<profile>"` on pins (comes with machines, M3b).

Manual checklist (home NAS has a saved password):
- [ ] `:net list` shows the NAS connection and its user.
- [ ] `:connect` on the NAS share with a wrong password: "logon failure… try 2 of 3".
- [ ] A second, valid NAS account: error 1219 → `a` (alias) opens the same folder; `:net
      list` shows both. `:net drop-all` removes the sentier one.
- [ ] Leave Remember unticked while testing: it would replace the saved NAS credential.

## M3b machines, slice 1: machine views (code complete 2026-11-06, manual check pending)

- `Loc::Machines` (`gm`, parent Locations) and `Loc::Machine(host)` (parent Machines), both
  built by the app (not listed by the worker). A bare name in Locations is a machine (its
  MACHINES section: pinned + 8 most recent). `Loc::admin_host()`: a machine view or any
  `\HOST\X$` path (not `IPC$`).
- Pins: `kind = "machine"` with `host`, optional `user` hint and `auth` (default `admin`) in
  the extra fields. `M` on a machine (or in its view), `m{key}` in its view, `'key` opens it.
- Recent machines: `%LOCALAPPDATA%\sentier\machines.tsv` (host, opened, last seen), kept
  `machines.recent_days` (30) days. Every machine view or admin-share path records it.
- Port-445 probe first (500 ms connect, 3 s with the name lookup): online with the round
  trip, or `offline (port 445 doesn't answer) · last seen 3 d ago`. An offline host's
  admin-share listing is cancelled and fails at once instead of waiting on SMB.
- Users: `NetWkstaUserEnum` (level 1, admin rights; people only), and the profile folders
  of `C$\Users` newest first (system / `adm-*` / Administrator skipped) as the fallback.
  Templates use: the one picked with `u` › logged on › the pin's `user` › newest profile.
- `[[machine.path]]` templates in config.toml (the spec's six by default); `{user}` and
  `{host}` expand; one key each jumps there from the view (`d w t r p`). Template folders are
  checked in the background (missing → greyed "missing"); `{user}` templates without a user
  say "who? u". `u` prompts for the user (Tab cycles the ones found). `C$` is last.
- `h` from the root of `\HOST\C$` of a known machine goes back to its view. `R` re-probes.
- Red `ADMIN SHARE · HOST · user` banner in the header on admin shares and machine views;
  status line `● online · 4 ms · templates for jdoe (u other user)`.

Next slice: the `<leader>m` machine picker (done, see slice 2 below). Still to come:
with file operations (M4/M6), the
"Delete 2 files on HOST (user's machine)?" confirmations and the audit log. With open-with
(M5): machine actions (RDP, Computer Management, Event Viewer, PowerShell, remotectl).

Manual checklist (office, admin account):
- [ ] `:cd \<a colleague's PC>\C$`: banner, probe, then `h` → machine view with the user.
- [ ] `d` / `w` / `t` jump to that user's folders; `u` picks another user.
- [ ] A laptop that's off: offline in under a second, no hang.
- [ ] `M` pins it; `gm` and `gl` list it.

## M3b machines, slice 2: the machine picker (code complete 2026-11-06, manual check pending)

- `<leader>m` (action `machine-picker`): pinned machines, then recent ones, then the
  inventory CSV and `source_cmd`, one row per host (`PC-042` and `pc-042.corp.example` meet).
  Rows carry the user and the other columns, so typing a person's name or a department
  finds the machine. Enter opens the machine view, `^a` pins an unpinned row, `^e`/`^t`/`^x`
  work on pinned ones.
- `[machines] inventory_csv = '…'`: a header row, then one machine a line (`,` or `;`,
  quoted fields). The host column is the first named host/hostname/computer/name/…, the user
  column user/username/owner/samaccountname/managedby/…; other values are details.
- `[machines] source_cmd = '…'`: run through `cmd /D /C` (as typed, no console window),
  killed after 20 s, stderr to the log. JSON: an array of objects (same key names, any
  case; `CN=Jane Doe,OU=…` becomes `Jane Doe`) or of host strings, or an object holding
  such an array.
- Both are read off the UI thread when the picker opens and cached 5 minutes; hosts arriving
  while the picker is open are added live. Errors show in the picker and the status bar.
- Dots and `is:online` / `is:offline` come from the port-445 probe: pinned and recent
  machines are probed on open, inventory ones when the cursor rests on them. `is:machine`
  and `on:<host>` work in the pin picker and Locations too.
- serde_json added (approved).

Manual checklist:
- [ ] Point `inventory_csv` at a real export; search a colleague's name; Enter opens it.
- [ ] `source_cmd` with `getad` (or `powershell -NoProfile -c "Get-ADComputer … | ConvertTo-Json"`).
- [ ] `is:offline` after a probe round; `^a` pins from the picker.

## M4 jobs: copy / move with robocopy, the jobs panel (code complete 2026-11-06, manual check pending)

- Selection: `Tab` (toggle + down, counts), `v`/`V` visual range with any motion (`y`, `x`/`d`,
  `Tab` keeps it as the selection, `Esc`/`v` leave), `Esc` clears. `▌` in the marker column,
  `3 selected · 4.31 GB` in the status line. Cleared on leaving the folder.
- `yy` / `x` (counts, selection or visual range) fill the register; `p` pastes without
  replacing (existing files are skipped and reported), `P` replaces (asks first), `.` pastes
  the last copy again here. A cut is pasted once. Pasting into the same folder or a folder
  into itself is refused.
- Backends (`jobs::plan`): a move within one volume is a rename (instant); files under
  256 MB (`[jobs] native_max_bytes`) copy natively (retrying locked files twice); anything
  else (folders, big files) goes to robocopy: one run per folder, one per source folder for
  files. `[jobs] robocopy_threads` adds `/MT:n`.
- robocopy: `/E /Z /R:2 /W:5 /BYTES /FP /TEE /UNILOG+:<job log>` (+ `/MOV`/`/MOVE`,
  `/XC /XN /XO` for `p`, `/IS /IT` for `P`). A `/L` pass first gives the totals (Scanning).
  stdout is decoded from the OEM code page (robocopy ignores `/UNICODE` when piped: `é` is
  0x82) and parsed by structure, not by English words, so a French Windows works too.
- Fixtures: `cargo run --example capture_robocopy` captured success, nothing to copy, mirror
  with extras, `/L`, `/MT`, locked file, access denied, missing source and an unreachable
  destination into `tests/fixtures/robocopy/` (paths scrubbed to `C:\capture`; marked
  `-text` in `.gitattributes`). `/MT` prints `0%`/`100%` per file only; handled.
- Exit codes are explained (`FAILED robocopy exit 9 = 1 (files copied) + 8 (some copies
  failed)`). Retries: a file seen again after an error; failed when the next file starts.
- Jobs panel (`J`, `<leader>j`; opens by itself after 5 s): list with phase, bar, ETA;
  detail with the phase chain, current file, bytes/files, speed (last speed while idle),
  elapsed, retries, errors, log path. **Finishing** is its own phase with a timer ("every
  byte is written… the old stuck at 99%"). Finished jobs show the report: counts, failed
  files with reasons. `c` cancels (kills robocopy), `r` retries only the failed files,
  `o` opens the log, `Enter`/`dd` dismisses. Reports are kept as JSON in
  `%LOCALAPPDATA%\sentier\jobs\` next to the logs until dismissed.
- AVEVA guard: a pasted folder whose top level matches every `[aveva] project_markers` glob
  (default `evars.bat` and `*000`) asks "looks like an AVEVA project: make sure no one has
  it open" first.
- Status line: `⇅ 1 job 38%` while jobs run.

Not yet: the in-use check (`NetFileEnum`, with presets in M4b), admin-share confirmations
naming the machine (with file operations), `dd` to the Recycle Bin (M6).

Manual checklist (office):
- [ ] Copy a big project folder from a share to local: Scanning → Copying → Finishing → Done.
- [ ] A file open in E3D: Retrying, then FAILED with the file and ERROR 32; `r` after closing.
- [ ] `c` mid-copy stops robocopy; the report says what was done.
- [ ] A folder with accents in names; the current file shows them correctly.
- [ ] Unplug / dead share as destination: fails with ERROR 53 explained, UI stays responsive.

Decision 2026-11-06: the "who has this project open" check (`NetFileEnum`, `jobs/inuse.rs`,
`check_in_use` on presets) is dropped: it can't work in the author's office environment.
The AVEVA marker guard stays as the only "is it open?" warning.

## File operations (section 5) (code complete 2026-11-06, manual check pending)

- `r` / `cw`: rename the item under the cursor; the prompt starts with the cursor before
  the extension. In Locations `r` still renames a pin. Case-only renames work; an existing
  target is refused.
- `a`: new file (`sub\x.txt` makes `sub` too; a trailing `\` makes a folder). `A`: new
  folder. Bad names (reserved `CON`/`COM1`, `:*?`, trailing dot/space) keep the prompt open
  with the reason. The cursor lands on what appeared.
- `dd`: to the Recycle Bin (`trash` crate) after a y/n; with counts (`3dd`), the selection
  or the visual range (`vjd`). On a share or a mapped drive the question says it's
  permanent (no Recycle Bin over the network), and the worker refuses a Recycle Bin delete
  on a network drive anyway (Windows would delete for good). `D`: permanent; type `yes`.
  Read-only files are made writable first, junctions are removed, never followed.
- `:rename`: the selection's names (or the whole folder's) in `$VISUAL`/`$EDITOR` (default
  `nvim`), one per line; a y/n lists the changes; applied through temporary names so swaps
  work; wrong line counts, duplicates and bad names rename nothing.
- Inline programs: the TUI suspends (input thread paused so it doesn't steal keys, raw mode
  and alternate screen left), the program runs in this terminal, then everything comes back
  and is fully redrawn. M5's inline openers will reuse this (`app::Suspend`).
- Admin shares (`\HOST\C$…`): every change (paste, rename, new, delete, bulk rename) asks
  first, naming the machine and its user ("On PC-042 (jdoe's machine): delete x.txt?
  Permanent…"), and is appended to `%LOCALAPPDATA%\sentier\audit.log` (time, host, user,
  operation, ok/FAILED, paths). `[machines] audit = false` turns the log off.

Manual checklist:
- [ ] `dd` a local file: it's in the Recycle Bin. `dd` on a share: the question says permanent.
- [ ] `:rename` opens Neovim in the same terminal; after `:wq` sentier comes back intact.
- [ ] `r` on `report.pdf`: cursor before `.pdf`.
- [ ] On a colleague's `C$`: rename asks naming the machine; `audit.log` has the line.
