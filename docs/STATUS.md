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

## Next: M2 (network)

- `platform`: `NetShareEnum` (server node listing), `WNetGetConnectionW` (mapped drive → UNC).
- The Locations root as its own screen per `docs/screens/02-locations.txt` (pinned folders /
  drives / shares sections, reachability dots, 60 s re-probe with a 2 s `metadata()` timeout).
  Root entries will need an explicit target `Loc` (servers can't be joined by name; see
  the comment in the `path.rs` round-trip test).
- Saved shares: a share is added the first time you browse it. This needs persistence, so
  decide where (pins.toml is M3).
- Test against a dead host and confirm the UI stays responsive.
