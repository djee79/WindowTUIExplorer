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

1. ~~`01-main` top bar shows `tab 1 of 2`.~~ Settled 2026-11-06: tabs added (section 1).
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

## M4b presets, slice 1: sync presets (code complete 2026-11-06, manual check pending)

- A `kind = "sync"` pin is a preset (`jobs/preset.rs`): `source`, `dest`, `mode`
  (`mirror` | `update`, default **update**), `exclude_dirs` (`/XD`), `exclude_files`
  (`/XF`). A preset whose folders are the same or nest is refused at load, reported like
  any broken pin, and kept in the file as written.
- Enter on it (picker, Locations, `'key`), `:sync <name>`, `<leader>rs` (the pin picker
  filtered to `is:sync`): a **dry run first**, `robocopy /MIR /L /R:0 /W:0` on the worker
  (Esc kills it). The preview (05-sync-preview) shows files to copy, how many replace a
  local copy, local files (and folders) **DELETED** in mirror or "kept" in update,
  unchanged; then the list worth a look (deletions in mirror, replacements in update).
  `Tab` lists every change, `j`/`k` `^d`/`^u` `gg`/`G` scroll, `u` flips mirror ↔ update for
  this run, `R` dry-runs again, `Enter` runs, `Esc`/`q` cancels ("nothing was touched").
  A failed dry run (dead share, access denied) shows robocopy's error and won't run.
  Nothing to do: Enter just closes.
- The dry run always uses `/MIR`, so it lists local-only files in both modes and `u`
  needs no second pass.
- `Shift-Enter` in a picker or `:sync! <name>` skips the preview for an **update** preset;
  mirror always previews.
- The run is a normal job (`sync · mirror` in the jobs panel), using the preview's totals
  (no second `/L` pass). When it ends, the outcome goes to
  `%LOCALAPPDATA%\sentier\presets.json` (keyed by source + dest, so a rename keeps it;
  merged entry by entry so a headless run isn't overwritten). The pin's preview pane shows
  `Last run  today 08:12 ✓ 3.80 GB in 2m41s` / `yesterday 17:03 ✗ failed (2 locked)`.
- robocopy: `/XD` / `/XF` support; a dry run never retries (`/R:0 /W:0`); the tracker
  keeps extra files' sizes and `*EXTRA Dir` folders.

Not yet: creating a sync preset from inside sentier (for now, a `[[pin]]` in pins.toml;
`Ctrl-e` edits name/tags/key), backup presets (slice 2), `sentier run` (slice 3). A
mapped drive and its UNC path (`P:\HIT` vs `\srv01\aveva\HIT`) aren't recognised as the
same folder by the nesting check.

Manual checklist (office):
- [ ] A real HIT → local preset in mirror: preview counts look right; Esc leaves D: untouched.
- [ ] `u` then Enter: update run; local-only files survive. Mirror run: they're gone.
- [ ] Preset on a dead share (`\10.255.255.1\x`): the dry run fails with ERROR 53 quickly
      (no 30 s retries), UI responsive, Esc works while it runs.
- [ ] `Shift-Enter` in WezTerm: does it arrive as Shift-Enter? (else `:sync!`).
- [ ] `last:` line after a run, and again after restarting sentier.

## M4b presets, slice 2: backup presets (code complete 2026-11-06, manual check pending)

- A `kind = "backup"` pin (`jobs/zip.rs`): `source`, `dest_dir` (local or UNC),
  `file_name` (default `{name}_{date:%Y-%m-%d_%H%M}.zip`; `{name}` is the source's last
  component, `{date:…}` any strftime format; `.zip` added if missing), `keep` (newest N,
  default 0 = keep all), `compression` (`store` | `fast` | `default` | `best`). Refused at
  load: dest_dir inside source, a bad template, unknown compression, negative keep. Zips
  next to the project (source inside dest_dir) are allowed.
- Enter / `'key` / `:backup <name>` / `<leader>rb`: one y/n naming the source, the zip,
  whether it stages, and the retention. Then one job, five steps:
  1. **staging**: robocopy `/L` for the size → free-space check (staging + zip; doubled
     when both are on one volume; refused with a clear message) → robocopy into
     `%LOCALAPPDATA%\sentier\staging\<job>`. Any failed file aborts: no zip is made from
     an incomplete copy. **Skipped** when the source is a sync preset's `dest`.
  2. **zipping** into `<dest_dir>\<name>.zip.partial` (zip64 for files ≥ 4 GB, empty
     folders kept, mtimes kept, `\?\` paths so deep projects work);
  3. **verifying**: reopen, entry count must match, every entry read back (CRC-checked),
     then renamed to the final name (`…_2.zip` if two runs land in the same minute);
  4. **retention**: deletes the oldest zips matching this preset's pattern (`HIT_*.zip`)
     beyond `keep`; never other files, never `.partial`s;
  5. **cleanup**: the staging folder goes.
  A failure or `c` removes the `.partial` and the staging folder.
- Jobs panel: `ZIPPING 2/5` in the list with % and ETA per step, `backup · HIT_….zip`; the
  detail shows the step chain (`Staging (skipped)` when direct) and the file being zipped /
  verified. The report shows the zip, "N entries read back, every CRC checked", and what
  retention removed. Last run on the pin: `today 02:00 ✓ 1.20 GB in 3m10s` (zip size).
- New crate: `zip` 9.4 (stack table), deflate only.

Not yet: the optional 7-Zip backend (`7z.exe`, `compression` → `-mx`); `sentier run`
(slice 3).

Manual checklist (office):
- [ ] HIT backup to a local folder and to a UNC dest_dir: zip opens in Explorer / 7-Zip.
- [ ] A full dest volume: refused before anything is copied, with the sizes.
- [ ] `c` during zipping: no `.partial` left, staging folder gone.
- [ ] keep = 2 after three runs: only the two newest `HIT_*.zip` remain, other files untouched.
- [ ] Source = a sync preset's local copy: the question says "zipped directly", no staging.

## M4b presets, slice 3: `sentier run` (code complete 2026-11-06, manual check pending)

M4b is code complete with this slice (the 7-Zip backend aside).

- `sentier run "<preset name>" [--allow-mirror] [--threads n]` (`jobs/headless.rs`): runs a
  sync or backup preset with no TUI. The name must match exactly (case aside), never by
  part; no name or an unknown one lists the presets. `sentier run --help`.
- A mirror sync runs as **update** unless `--allow-mirror` is given (it says so).
- Same log and report as the TUI (`%LOCALAPPDATA%\sentier\jobs\`, so the jobs panel shows
  it next time) and the same last-run entry in `presets.json` (the pin shows it).
- Prints a line per milestone (scanned, each backup step, zip verified, retention, the
  result, failed files), not per file.
- Exit codes, robocopy-style: 0–7 ok (a sync returns robocopy's own bits; a backup 1),
  8 failed or cancelled (or robocopy's failure code), 16 couldn't run (no such preset,
  broken pin, no pins.toml).
- A folder literally named `run` is still reachable as `sentier .\run`.

Nightly backup from Task Scheduler (PowerShell, once):

```powershell
$a = New-ScheduledTaskAction -Execute "sentier.exe" -Argument 'run "HIT backup"'
$t = New-ScheduledTaskTrigger -Daily -At 2am
Register-ScheduledTask -TaskName "sentier HIT backup" -Action $a -Trigger $t
```

Manual checklist:
- [ ] The task above runs at 2 a.m.; Task Scheduler's "Last Run Result" is 0x1.
- [ ] A broken share: the task's result is 8 or more, and the report shows in `J` next day.
- [ ] `sentier run "HIT → local"` on a mirror preset doesn't delete local extras.

## M4b finished: presets made in the app, 7-Zip engine (code complete 2026-11-06)

**M4b is code complete.** Nothing in section 9 is left to build (the in-use check was
dropped on purpose; the per-share index is a stretch goal of M2a).

- **Presets are made and edited in sentier**, no TOML by hand:
  - `:sync new` / `:backup new` in the folder to copy from. Prompts: name (`HIT → local`,
    `HIT backup`) → source (prefilled) → for a sync: local copy, then mode (`update` |
    `mirror`, the note spells out what mirror deletes); for a backup: zips folder, keep
    (default 10), and the engine when 7-Zip is installed → tags → key.
  - Path fields take any path form, `~`, or a path relative to the cwd, and **Tab
    completes folders** like `:cd`.
  - Each field is checked as it's entered (dest inside the source, zips inside the
    source, keep not a number…): the prompt stays open with the reason. The whole preset
    is then checked as the loader and `sentier run` see it. `Esc` at any step saves nothing.
  - `^e` on a preset in a picker walks the same fields, prefilled.
- **7-Zip engine** (`engine = "7zip"` on a backup preset; default `builtin`):
  `7z a -tzip -mx=<0|1|5|9> -mmt=on`, run in the source folder, progress from its `NN%`
  output, everything else into the job log, cancel kills it. Exit 1 (a file it couldn't
  read) fails the backup. 7-Zip missing → built-in writer, with a note in the log.
  Found on PATH or in Program Files, once, on the worker at startup.
  - The "offer": when 7-Zip is installed and a preset uses the built-in writer, the backup
    question says so; the wizard asks for the engine only when 7-Zip is there.
- Verification now checks the **file** count (folder entries differ between writers) and
  still reads every entry back with its CRC.
- Picker preview of a backup pin: keep, engine, last run.
- Tests can no longer read or write the real `pins.toml` / `shares.toml`: `store_path()`
  points at a temp folder under `cargo test`.

Manual checklist:
- [ ] `:sync new` in `\srv01\project\HIT`, Tab to `D:\aveva\local\HIT`, mirror: the pin
      appears in `<leader>p` with its fields; Enter previews.
- [ ] `:backup new` with 7-Zip installed: engine 7zip; a big project zips noticeably faster
      than builtin; the zip opens in Explorer.
- [ ] `^e` on that backup: change keep; `pins.toml` shows the new value.

## M5 open-with, slice 1: rules, openers, `o` / `O` (code complete 2026-11-06, manual check pending)

- `%APPDATA%\sentier\open.toml` (`open/rules.rs`): `[opener.<name>]` (`desc`, `run` with
  `{path} {paths} {dir} {name} {stem} {ext}`, `mode = inline | detached | tab`,
  `for = all | each`), `[[open.rule]]` (`match` glob on the name, or `"dir"`; `use`, in
  order), `[open] remember / o_uses_last`. First matching rule wins; built-in rules stay
  last (folders: enter, explorer, terminal-here, copy-path, copy-unc; files: default,
  reveal, copy-path, copy-unc), so with no file at all everything opens as before.
  Bad openers / rules / unknown names are skipped and reported in the status bar + log.
- Built-in openers: `default` (ShellExecute), `reveal` (`explorer /select,"…"`),
  `explorer`, `terminal-here` (`wezterm cli spawn --cwd`), `enter`, `copy-path`,
  `copy-unc` (mapped drive → `\server\share\…`), `openwith` (`SHOpenWithDialog`, on its
  own thread), `properties` (`SHObjectProperties`). `pin:<name>` runs an `app` pin with
  the paths appended.
- Programs are found once at startup on the worker: a path as given, else PATH (PATHEXT),
  else `App Paths` (HKCU, then HKLM). Not found → greyed "not found: …" in the menu.
- `o` / `Enter` / `l` on a file: the rule's first opener, on the selection's files when
  there is one (`for = all` → one process). Folders still enter.
- `O`: the menu (09-open-with): configured section with the rule's pattern, then Open
  with… and Properties. `1`–`9`, `j`/`k` / arrows / `^n` `^p`, Enter, typing filters
  (`Esc` clears the filter, then closes). The pick is remembered per extension
  (`%LOCALAPPDATA%\sentier\open_last.json`): `★ last`, moved to the top; `o` uses it only
  with `o_uses_last = true`.
- Inline openers suspend the TUI (as `:rename` does) and re-list the folder after;
  detached ones start with no console window, output in `logs\openers.log`.
- New keys: `O`, `gR` reveal, `Alt-Enter` Properties, `yp` / `yP` / `yn` (copy path / UNC
  path / name; M6's path utilities, pulled forward), `gx` Explorer here, `gw` WezTerm tab
  here. New crate: `arboard` (stack table), no default features.

Slice 2 below adds "Windows suggests" and the default app's name.

Manual checklist:
- [ ] `Alt-Enter` on a file and on a folder: the Properties sheet appears and stays open.
- [ ] `O` → Open with…: Windows' chooser; "always use this app" works.
- [ ] `gR` on a file on a share: Explorer opens with it selected.
- [ ] An `inline` nvim opener: Neovim in this terminal, sentier back intact after `:q`.
- [ ] `yP` on a mapped drive file pastes `\server\share\…` into Teams.
- [ ] `gw` inside WezTerm: a new tab in this folder.

## M5 open-with, slice 2: Windows suggests (code complete 2026-11-06, manual check pending)

- The `O` menu has a **"Windows suggests (registered for .pdf)"** section: the apps
  `SHAssocEnumHandlers(ext, ASSOC_FILTER_RECOMMENDED)` returns, the same list as
  Explorer's Open with submenu, so installed apps appear with no config. Picking one runs
  `IAssocHandler::Invoke` on each selected file's data object (`IShellItem` →
  `BHID_DataObject`).
- COM objects never cross threads: the lookup runs on its own STA thread and sends back
  only (name, display name); invoking finds the handler again by name on a fresh thread.
- Asked once per extension per session; until it answers, the menu shows a greyed
  "looking up…" row, then fills in with the cursor kept on its item.
- "Default app" shows the app's name (`AssocQueryStringW`, `ASSOCSTR_FRIENDLYAPPNAME`).
  On this French Windows: `.txt` → "Bloc-notes", plus Visual Studio Code (checked by a
  test against the real registry).
- A Windows app can be remembered as `★ last` like any opener.

M5's open-with part (section 13) is code complete. Next: the launcher (section 6).

Manual checklist:
- [ ] `O` on a PDF: the Windows section lists the same apps as Explorer's Open with; picking
      one opens the file in it (also a file on a share).
- [ ] With 3 PDFs selected, a Windows app opens all three.

## M5 launcher, slice 1: launcher picker, frecency, app and web pins (code complete 2026-11-06, manual check pending)

Ported from wtm (`src/bar.rs`: `usage_score`, `run_command`, `expand_env`, the ranking
and browse view).

- `<leader><leader>` (and `Ctrl-Space` where the terminal delivers it): the launcher, a
  picker over every pin and view. Same filters as the pin picker (`#tag`, `is:`, `on:`).
- Ranking with a query: wtm's tiers (name prefix › tag prefix › substring › fuzzy), then
  frecency, then A→Z. With no query: "frequent" (the 5 most used, when ≥ 2 were used and
  there are groups), then the untagged, then each first tag A→Z, under headers. The pin
  picker also sorts by frecency within a tier now.
- Frecency (`frecency.rs`): `%LOCALAPPDATA%\sentier\usage.tsv` in wtm's format
  (`count\tlast\tkey`), score = min(count, 100) × 8 / 4 / 2 / 1 by recency (< 1 h, < 1 d,
  < 7 d). Keyed by what the pin opens (`command args` like wtm, the URL, the path), so
  renames keep history. Every pin open counts (launcher, pickers, `'key`, Locations).
- App pins run through `ShellExecuteW("open", command, args, dir)` with `%ENV%` expanded
  in each (`ExpandEnvironmentStringsW`): `.exe`, `.bat`, `.lnk`, documents.
- Web pins: `<browser> --app=<url>` (+ `--profile-directory=` from `profile`), browser
  `edge` (default) / `chrome` / `brave` / `vivaldi` / a path, found through PATH and
  `App Paths`.
- Nothing matches: Enter runs the typed text (`notepad x.txt`, a URL, a quoted path).
- `Ctrl-e` / `Ctrl-x` / `Ctrl-t` from the launcher come back to the launcher.

Not yet (slice 2): `:import-wtm` (and the first-run offer), "+ pin URL…", the Start Menu
scan, the `evars.bat` context action. Recording every visited folder (for `<leader>fr`)
comes with the M6 pickers.

Manual checklist:
- [ ] `Space Space`: pins listed under frequent / headers; typing narrows instantly.
- [ ] A web pin opens a chromeless Edge window; `browser = "chrome"` uses Chrome.
- [ ] An app pin with `%APPDATA%` in its command runs.
- [ ] Does `Ctrl-Space` arrive in WezTerm on Windows? (else `Space Space`).

## M5 launcher, slice 2: wtm import, pin a URL, Start Menu (code complete 2026-11-06, manual check pending)

**M5 is code complete.** The `evars.bat` context action (section 6) was dropped by the
user ("no need for that").

- `:import-wtm` (`launcher/entry.rs`): `%APPDATA%\wtm\config.toml`'s `[[launcher]]`
  entries become pins after one y/n: `--app=URL` on a browser exe → web pin (`browser`
  kept unless Edge), a folder → folder pin, the rest → app pin (`command`, `args`, `dir`);
  `group` → first tag (`Web Apps` → `web-apps`); wtm's global `key` dropped. Entries
  already pinned (same target, or same kind + name) are skipped and counted. wtm's
  `usage.tsv` comes along (same key for apps; web keys re-keyed to the URL), so the
  launcher's "frequent" is right from the start.
- First run (no `pins.toml` yet) with a wtm config: the same question is offered once;
  `n` writes an empty `pins.toml` so it isn't asked again.
- Web and app pins are made and edited in the app (no TOML by hand): `:pin url [URL]` or
  `^a` in the launcher (the typed text as the URL) → name (from the host) → url (checked,
  `https://` added) → browser (edge / chrome / brave / vivaldi / a path) → tags → key.
  `^e` on an app pin walks command → arguments → start-in folder (Tab completes).
- `^s` in the launcher (`launcher/startmenu.rs`, wtm's `scan_start_menu`): the Start
  Menu's apps in or out of the list (`[start]`), scanned once on the worker: per-user then
  all-users `.lnk`, 4 deep, uninstallers skipped, deduped by name. Enter runs one (and
  counts it for frecency, under the same key its pin would have); `^a` pins it as an app.

Manual checklist:
- [ ] `:import-wtm` with the real wtm config (26 entries here): the question's counts look
      right; imported web apps open as app windows; groups show as headers in `Space Space`.
- [ ] `Space Space`, `^s`: Start Menu apps appear; Enter starts one; `^a` pins it.
- [ ] `:pin url` for a real web app, Chrome as browser.

## Discoverability: nothing to remember (2026-11-06)

User feedback: "I'll never remember these commands by heart". Commands now show up where
you already are:

- **Launcher "+ …" rows** (`app/launcher.rs` `ACTIONS`): `+ Pin a web app (URL)…`,
  `+ Show / hide Start Menu apps`, `+ Import from wtm…`, `+ New sync / backup preset from
  this folder…`. Last in the browse view under "actions"; found by typing (`pin`, `wtm`,
  `sync`). Shown only for a real (substring) match, never a loose fuzzy one, so typed text
  that matches nothing still runs on Enter.
- **`Space n` "new…" group** in which-key: `nu` web app pin, `ns` sync preset, `nb` backup
  preset, `ni` import wtm.
- **`:` hints**: one table, `keymap::COMMANDS` (command, arguments, what it does). The
  empty command line lists them (`cd · pin · import-wtm · …`); typing narrows to
  `pin url [URL] — pin a web app`. Every listed command is checked to parse.
- **`g?` help** has a ": commands" section from the same table.

## Yes/no questions are popups (Yazi-style) (2026-11-06)

User feedback: typing `yes` for `D` was annoying; wanted a popup to confirm, like Yazi.

- Every y/n question (delete, Recycle Bin, paste-replace, backup, wtm import, tag and
  view changes, unpin, admin-share changes) is a centred popup (`ui/dialog.rs`): title,
  the question, what it touches (first 8, "… and N more"), `[ Yes ]  [ No ]`.
- Keys: `y` yes, `n` / `Esc` / `q` no, `h` `l` `Tab` ← → pick a button, Enter presses the
  highlighted one. Other keys do nothing (a stray key is no longer an answer).
- Destructive ones (`D`, `dd` on a share, paste-replace, deleting a view or a tag) are
  red and start on **No**: a stray Enter keeps everything; `y` (or `l` + Enter) confirms.
- `D` no longer asks to type `yes` (the `TypeYes` prompt is gone).

## M6, slice A: the finder pickers (code complete 2026-11-06, manual check pending)

- `Space f f` files below the cwd: the search walker (`type:f`; `depth:4` on a share,
  `^l` lifts it) streams them in while you type; `matched / N scanned` with a spinner.
- `Space f r` recent folders: every folder opened is recorded (`dir:` keys in
  usage.tsv, written at most every 5 s and at exit), most used first, then the cwd's
  subfolders.
- `Space /` grep: the text is a content search (literal, `re:`, `ext:`…), walked again
  250 ms after the last key; rows show `N matches`.
- `Space :` every action (by what it does, with its keys) and every `:` command; Enter
  runs it, a command that needs an argument opens `:` with it typed. `Space s k` the key
  bindings only.
- On file / folder rows: Enter opens (its folder with the cursor on it, then the first
  opener; a folder: go there), `^o` open-with menu, `^v` reveal in Explorer, `^y` copy
  path, `^a` pin, `Tab` choose several → Enter yanks them (`p` pastes as a copy).
- `:picker files | dirs | grep | commands | keymaps | pins | launcher | tags | machines`
  (so keymap.toml's `run = "picker files"` works); the finders are in the `:` hints / `g?`.

## M6, slice B: CLI, config overrides, `go` / `Space e` (code complete 2026-11-06)

**All milestones (M1–M6) are code complete.** What remains is the manual check of each
milestone at the office (the checklists above) and the few items marked "not yet".

- CLI for other tools (`cli.rs`), no TUI, exit 0 / 1 refused / 2 bad usage:
  - `sentier pin <path|url> [--name N] [--tag T]… [--key K]`: a folder or file (resolved
    against the current folder; `.exe/.bat/.lnk` become apps) or a web address; refuses
    one already pinned. Written atomically.
  - `sentier pins [--query "#aveva is:unc"] [--json]`: the pickers' filters (`#tag`,
    `!#tag`, `is:`, `on:`, words); `--json` is every field as in pins.toml.
  - `sentier open <pin name>`: exact name; folders / files in their Windows app, apps,
    web apps, a machine's `C$`; presets point to `sentier run`. Counts for frecency.
  - A folder named `pin`, `pins`, `open` or `run` opens as `sentier .\pin`.
- A running sentier reloads pins.toml when something else changes it (checked every 2 s,
  its own saves recognised and skipped; held while a pin is being edited): "pins.toml
  changed: reloaded".
- config.toml: `show_hidden`, `numbers` (hybrid | relative | absolute | off),
  `[timeouts] local_ms / network_ms`, `[preview] protocol / max_network_mb`, `editor`.
  Timeouts and the protocol are read before the TUI starts (1 s cap, as %APPDATA% can be
  a redirected network folder). `preview.cell_px` and `preview.cache_mb` load but aren't
  used yet (there is no on-disk thumbnail cache yet).
- `go` / `Space e`: this folder in the editor (config `editor`, $VISUAL, $EDITOR, nvim), in
  this terminal; `:rename` uses the same editor.
- Tests can no longer read or write the user's config.toml, keymap.toml or machines.tsv
  either (temp paths under `cargo test`).

## Design drop adopted; M4c bulk rename, slices 1–3 (code complete 2026-11-06, manual check pending)

Design drop (docs/sentier-screens/, gitignored): section 16 + screen 12 adopted into
CLAUDE.md with placeholder names. Decisions: the panel opens with `r` / `cw` on a selection
or visual range, or `:rename` (`R` stays retry / refresh); the Neovim flow is
`:rename-edit`; `fancy-regex` added; the drop's lost `serde_json` row kept.

- Slice 1, engine (`rename/`): PowerRename's search (literal / regex / Boost with
  `fancy-regex`), options, tokens (enumeration, random, `$YYYY`…`$f` with Windows-locale
  month / day names, `$CAMERA_*` / `$DATE_TAKEN_*`, extras `${parent} ${name} ${ext}
  ${pdf_title} ${n;reset=folder}`), and the plan with its checks. `$1` means group 1 as in
  PowerRename (the regex crate would read `$1_R` as a group named `1_R`).
- Slice 2, the panel (`app/rename.rs`, `ui/rename.rs`, screen 12): Search for / Replace with
  (Tab switches), live preview table with the relative gutter and Original │ Renamed │
  Status, the toggles in options mode (`Esc`: `r b a c n f d t e x m o`), `Space` excludes /
  includes, `j/k gg G ^d ^u`. Enter refuses while anything is blocked, else asks
  ("Rename 8 items? (2 skipped)", the popup lists `a → b`); no comes back to the panel,
  yes applies through the existing two-phase bulk rename (swaps, case-only), with the
  admin-share confirmation and audit as for any change. Options are remembered for the
  next time (PowerRename restores its flags).

- Slice 3 (`rename/apply.rs`, `presets.rs`, `meta.rs`):
  - **Undo.** Every batch (panel or `:rename-edit`) is journaled in
    `%LOCALAPPDATA%\sentier\rename-journal\` (one JSON file per batch: where each item is
    now and its old name; newest 100 kept). `u` undoes the newest batch in the current
    folder or below; `:undo-rename [n]` the n newest anywhere. Both read the journal,
    then ask ("Undo the last bulk rename (5 items, today 10:42)?", listing `new → old`,
    noting items no longer there). What couldn't be reverted stays journaled.
  - **Apply order.** Deepest first, each level two-phase, so a folder and its contents
    rename in one batch (and undo the same way).
  - **Subfolders.** `s` walks into the selected folders off-thread (links not followed,
    hidden items only when shown, 20 000-item cap); each folder's contents are listed
    right after it, with the collision check per folder.
  - **Presets and history.** `^s` saves search + replace + options under a name
    (`[[rename.preset]]` in `%APPDATA%\sentier\rename.toml`, hand-editable; same name
    replaces). `^o` lists them (Enter loads, `x` twice deletes). `^p` / `^n` in a field
    step through its last 50 values. Options and history persist in
    `%LOCALAPPDATA%\sentier\rename-state.toml`.
  - **Values from the files.** Once the Replace text uses a photo token or `${pdf_title}`,
    or the date source is "EXIF date taken", the files are read off-thread (8 at a time,
    results stream in): EXIF via `kamadak-exif` (camera, lens, ISO, aperture `f2.8`,
    shutter `1-250`, focal, flash, bias, orientation, colour space, size, author,
    copyright, GPS as decimal degrees, date taken), PDF title from XMP `dc:title` or the
    Info dictionary (first and last 512 KB only; titles inside compressed object streams
    aren't found). Values are made filename-safe.

Manual check: rename photos with `$DATE_TAKEN_YYYY-$DATE_TAKEN_MM-$DATE_TAKEN_DD_${padding=3}`
(enumerate on), a folder of PDFs with `${pdf_title}`, a folder tree with `s`, then `u`.

## Tabs (code complete 2026-11-06, manual check pending)

`t` new tab on this folder · `Alt-1…9` / `{n}gt` tab n · `gt` / `gT` next / previous ·
`q` / `:q` / `:tabclose` closes a tab (`q` quits on the last; `ZQ` / `:qa` quit
everything) · `:tabnew [path]`, `:tabnext [n]`, `:tabprevious`. Plain digits stay vim
counts (user's choice over Yazi's plain `1…9`). Per tab: folder, cursor, scroll, `H`/`L`
history, `''`, selection, filter (`app/tabs.rs`). Shared: listings cache, jobs, pins,
search results. Up to 9. The header shows ` 1 HIT  2 Downloads ` from two tabs on. Listed
in `g?`, which-key (`g`), and the `:` hints.

Manual check: `Alt-1…9` must arrive as Alt+digit under WezTerm / ConPTY
(`cargo run --example probe_keys`); `gt` / `3gt` always work if not.

## `g?` help overlay redesign (code complete 2026-11-06, manual check pending)

Per docs/HELP_OVERLAY_SPEC.md (user decisions: sentier's own colours, not Tokyo Night; tab
commands under ": GO & CONNECT", a TABS key section added).

- Data: every action has a `g?` section (`Action::group()`, an exhaustive match, so a new
  action must pick one; remaps and keymap.toml additions follow their action). `:` commands
  are structs (`keymap::Command`: name, args, desc, `CmdGroup`, `KeyHint`); the key beside a
  command comes from the live keymap (`Keymap::shortest`: described binding first, then
  fewest keys), so remaps show. Non-remappable keys (`C-c`, prompt `Esc`) are `FIXED`.
- `src/help.rs` (pure, tested): sections, chips (`␣ff`, `↵`, `C-d`, `g Space`, `'‹c›`,
  `A-1…9`), joined rows (`zz zt zb`, `n N`…), the filter, wrapping with a hanging indent,
  and the layout (3 binding columns of 46 + commands when ≥ ~200 wide, then 2, 1, then
  one stacked column; sections balanced, never split).
- `ui/help.rs` draws it; `app/help.rs` handles `/` filter, `j k C-n C-p C-d C-u` scroll,
  any other key closes. `<leader>sk` / `<leader>:` list the same rows (with their section).
- `[theme] key` / `header` in config.toml recolour chips and headers.
- `D`'s description no longer says "type yes" (it's a popup now). The `:` commands list
  gained `:picker keymaps`; "(also …)" suffixes are gone (the key column says it).

Manual check: `g?` at full width and in a small split; `/` then type; a keymap.toml remap
shows in both the chips and the command hints.

## Jobs panel redesign, slice A: layout + data (code complete 2026-11-06, manual check pending)

Per docs/JOBS_PANEL_SPEC.md (user decisions: sentier's colours; sentier will own retries,
slice B). Also: the panel now opens after 0.5 s instead of 5 s (a paste felt laggy).

- Engine (`jobs/mod.rs`): when each phase began (`stage_log` → `stage_times`, Retrying
  counted as Copying), one speed sample per second (last 64) and a ~10 s smoothed speed
  that the ETA now uses, when bytes last moved, and an ACTIVITY tail (scan / start / copy
  / retry n / error / done, last 50). The report keeps the source, stage times, samples
  and activity (`serde(default)`: old reports still load).
- Panel (`ui/jobs.rs`): list 64 cols with RUNNING cards (3 lines: title + status word,
  40-cell bar + % + ETA, mode · engine · files · bytes · speed) and FINISHED entries
  (✓ ✗ –, finish time, files · mode (engine) → dest); the detail pane: stepper with
  durations, a wide eighth-block bar, bytes + ETA, THROUGHPUT chart beside STATUS
  (stacked when narrow), PATHS (wrapped, never cut), ACTIVITY (shrinks first). Below
  110 cols the list sits above the detail at the height it needs. The keys stay on the
  status bar's hint row; the JOBS line colours its counts.
- robocopy's "skipped" (already there, unchanged) shows as "unchanged", not a warning.
- Failed files list the path, then `error 32 · <Windows message>` wrapped under it.

Slice B (next): sentier owns retries (`/R:0`, locked files retried at the end), stall
detection (> 10 s without bytes) with the banner, `s` skip / `R` retry now, `!` for jobs
that finished with files skipped on purpose.

## Jobs panel redesign, slice B: sentier owns retries, stalls, pause (code complete 2026-11-06, manual check pending)

- robocopy now always runs with `/R:0 /W:0`: a locked file (error 32/33) fails at once and
  everything else is copied first. The runner (`jobs/run.rs`) queues the locked files and,
  after the main pass, tries each one again (a one-file robocopy into its own folder, never
  `/MIR`; or a native copy) every 10 s, up to 5 times. Events: `Waiting` (attempt, next
  try), `Recovered`, `Skipped`, `GaveUp`. The tracker drops a recovered file from the
  failed list and never lists a file twice. A job whose locked files all went through
  ends DONE (robocopy's stale 8 bit is cleared); one still locked ends FAILED with it.
- `jobs::Control` (cancel + paused / skip / retry-now atomic flags) replaces the bare
  cancel token for jobs and backups; the UI never waits on it.
- Keys in the panel: `p` pause / resume, `s` skip the locked file being waited on, `R`
  try it now (the hint row shows `s` / `R` only while waiting). Pause stops robocopy (no
  locks kept on the server) and runs the step again on resume (`/Z` resumes a half-copied
  file); the native backend pauses between files; a retry countdown pauses too.
- Stall detection: no bytes for 10 s while copying / finishing, or waiting on a locked
  file → STALLED (yellow), `◐` on the stepper with the stall time, and a banner with the
  most specific cause (waiting to retry X: the Windows message, error N, try k of 5 in
  m:ss; finishing; no bytes while robocopy runs) and the keys that help. The status bar
  shows `⚠ 1 job stalled · 99%` (or `‖ 1 job paused`) even with the panel closed.
- Finished with files left out: `!` (yellow), `N skipped`, and a SKIPPED list in the
  report (`skipped_files`, `serde(default)`).
- `sentier run` (headless) gets the same retries: a nightly backup now waits up to ~50 s
  for a locked file before reporting it.
- Tests drive real robocopy against a file held open with no sharing (error 32): released
  during the waits → copied on retry 2; `R` then `s` → left out, job DONE with 1 skipped;
  locked throughout → FAILED after 5 tries, the rest copied; pause / resume / cancel while
  paused.

Manual check: copy a folder from a share while one file in it is open in another program
(Excel keeps .xlsx locked); watch the banner, try `R`, close the program, see it go
through; again with `s`. Pause a big copy with `p`, resume, and compare the result.

## Open in the editor, no config (code complete 2026-11-06, manual check pending)

- Built-in opener `editor`: the editor from config.toml (else `$VISUAL` / `$EDITOR`, else
  `nvim`) on the file(s): inline in this terminal for terminal editors (nvim, vim, hx,
  micro, nano…) or `code --wait`, detached otherwise. Always in the `O` menu, files and
  folders. `go` / `Space e` use the same code.
- Built-in GUI openers `notepadpp` and `vscode`, shown only when found (App Paths, PATH,
  or `%ProgramFiles%\<name>\<name>.exe`); also usable by name in open.toml rules.
- Without open.toml: text and code files (a long list incl. PML) open in the editor with
  `o` / Enter; `O` offers Notepad++ / VS Code / Windows' apps; other files keep opening
  in their Windows app with the editors in `O`. ★ last + remember keep the per-extension
  choice on top of the menu.
- Tests can't start real programs any more (`spawn_detached` and launches are no-ops under
  `cargo test`): one test run opened a VS Code window before this guard existed.

Manual check: `o` on a .pmlmac → Neovim in the terminal, `:q` → back; `O` → Notepad++.

## Fix: VS Code's log on top of the TUI; Ctrl-L redraw (2026-11-06)

Opening a file in VS Code from "Windows suggests" (IAssocHandler::Invoke, in sentier's
own process) let Code.exe, an Electron app, attach to its parent's console (ours) and
print its main-process log over the screen. sentier now sets `ELECTRON_NO_ATTACH_CONSOLE=1`
first thing in `main` (inherited by everything it starts: VS Code, Teams, Slack…). Also
new: `Ctrl-L` / `:redraw` clears and redraws the whole screen, for anything else that
writes on the terminal. Manual check: `O` on a .txt → Visual Studio Code (Windows
suggests), close VS Code, the screen stays clean.

## Presets made by going there; only the selection; leave files out in the preview (code complete 2026-11-06, manual check pending)

User's ask: "we're in a file explorer: I should navigate to the source and destination
and the files to sync, not paste paths."

- Pick mode (`app/pick.rs`): the wizard's folder fields (Source, Dest, zips folder) no
  longer open a text prompt; the explorer works as usual under a `PICK  sync HIT → ?`
  banner (header, yellow) with a hint row (`p use this folder · Esc cancel · hjkl ' gl :cd
  A`). `p` takes the folder you're in and runs the same checks as before (same folder,
  nested folders, zips into what they back up), refusing in place; `Esc` (once there's no
  filter / selection / search highlight to clear) drops the preset.
- New presets: the source is where you are; the destination is picked first; mode (or
  keep / engine) next; the name last, suggested from both folders; tags; key. `Ctrl-e`
  picks each folder starting where it is (`p` keeps it).
- A selection makes `include` (folders end in `\`): `SyncPreset::steps()` gives one robocopy
  step per folder (mirror stays within it) plus one for the files (a file filter: copied,
  never purged). The dry run runs every step into one tracker (`run::dry_run_steps`,
  summaries added up); the real run and `sentier run` use the same steps.
- Preview: the list has a cursor (highlighted); `x` leaves that file out, `X` its folder,
  as full-path `/XF` / `/XD` in the pin (verified with a robocopy probe: full paths
  exclude a source file / folder and keep a dest-only file / folder from `/MIR`'s purge);
  the dry run is redone on the same list and row; `E` brings everything back. The key row
  wraps onto two lines when it's long.
- Tests: the whole wizard by navigation (refusals, edit, backup, include, Esc), and `x`
  / `X` / `E` with a real robocopy run honouring them.

Manual check: in a project folder, `Tab` two folders, `Space n s`, go to a local folder,
`p`, Enter ×4; Enter on the pin → preview lists only those; `Tab`, `x` a file, run.

## `f` speaks the search language (code complete 2026-11-06, manual check pending)

`f` (filter this folder) now understands what `s` does, in place: `*.pdf`, `ext:pdf,dwg`,
`size:>10mb`, `mtime:<7d`, `type:d` / `type:f`, `!old`, `re:…`, `hidden:yes`, combined with
text (`ext:pdf rev3`). Plain text keeps its old meaning (a substring of the name, not the
search's fuzzy match). A half-typed query (`size:>`) says why under the prompt and
filters as plain text meanwhile; the empty prompt lists what it understands.
`search/filter.rs` (pure, reuses the search's parser and name matcher).
