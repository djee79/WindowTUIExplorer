# Jobs panel redesign (`J` / `␣j`) — implementation spec

Redesign of sentier's copy/move jobs panel. Visual reference: `jobs-panel-mockup.dc.html`
(HTML mockup with two states, `copying` and `stalled`; open it for intent, ignore the
`<x-dc>`/`sc-for` template syntax). Palette and conventions match `HELP_OVERLAY_SPEC.md`.
Everything below is buildable on a character grid.

## Goals

1. Use the detail pane properly: a big progress bar, speed graph, status, paths, and live log tail.
2. **Never sit silently at 99%.** When bytes stop moving, say why (locked file, retry wait,
   finishing, flushing) and offer actions. This is the headline feature.
3. Split the list into Running and Finished; make finished jobs compact and readable.
4. One context-aware hint row instead of duplicated hints in the pane and the footer.

## Screen layout

```
 sentier  1 test  2 site-survey.avi   C:\Users\jdoe\Downloads\test        ← tab bar (unchanged)
╭─ Jobs ───────────────────── 4 ─╮ ╭─ site-survey.avi → test ──── copy · robocopy ─╮
│ RUNNING 1 ──────────────────── │ │ ✓ Scanning 0:01 ──── ● Copying 0:32 ──── ○ Finishing ──── ○ Done │
│▌site-survey… COPYING │ │ [stall banner, only when stalled]                         │
│ ████████▍░░░░░░░░ 44%  ETA 0:37│ │ ███████████████████████████████████████▍░░░░░░░░░  44%     │
│ copy · robocopy · 1 file · …   │ │ 306.12 MB of 700.49 MB                         ETA 0:37   │
│                                │ │ THROUGHPUT ─────────── 10.6 now · 9.6 avg │ STATUS ────── │
│ FINISHED 3 ───────── ↵ dismiss │ │ 12│      ▂▅▇█▇██▇█▇██▇██▇█…                  │ Speed  …     │
│ ✓ 0xProto.zip → test     14:32 │ │  0└──────────────────────                 │ Files  …     │
│   1 file · move (rename) → …   │ │ PATHS ──────────────────────────────────────────────────── │
│ …                              │ │ From / To / Log (o open)                                   │
│                                │ │ ACTIVITY ─────────────────────────────── tail of the log ─ │
╰────────────────────────────────╯ ╰────────────────────────────────────────────────────────────╯
 JOBS 1 running · 3 done · reports stay in %LOCALAPPDATA%\sentier\jobs\ until dismissed   ⇅ 1 job · 44%
 j k pick job   c cancel   r retry failed   o log   ↵ dismiss   J close
```

- List pane: fixed **64 cols**. Detail pane: the rest. Both rounded borders with titles on the border
  (left title, right meta: list = job count, detail = `mode · engine`).
- Detail pane sections are separated by one blank line; section header = `TITLE ` + `─` rule
  (same style as the help overlay), optional dim text right-aligned on the rule.

### Narrow terminals

- < ~150 cols: the THROUGHPUT and STATUS blocks stack instead of sitting side by side.
- < ~110 cols: list and detail stack vertically (list shows the running jobs + 3 finished, then detail).
- The detail pane scrolls if needed. ACTIVITY is the section that shrinks first.

## Job list (left pane)

**RUNNING n** section. Each running job is a 3-line card with a subtle background (`#1f2335`):
- Selected job: `▌` in the job's status color in column 0.
- Line 1: `src → dst` (ellipsize the middle of long names, `→` dim), status word right-aligned,
  bold, in status color: `COPYING` orange, `MOVING` orange, `FINISHING` orange, `STALLED` yellow,
  `FAILED` red.
- Line 2: 40-cell bar + bold `44%` + dim `ETA 0:37` (stalled: `retry 3 in 0:18`).
- Line 3 dim: `mode · engine · N file(s) · done / total size · speed`.

**FINISHED n** section, `↵ dismiss` dim on the rule. Each finished job is 2 lines:
- Line 1: green `✓` (red `✗` if failed, yellow `!` if completed with skipped files) + `src → dst`,
  finish time right-aligned dim.
- Line 2 dim, indented 2: `N file(s) · mode (engine) → destination path`.
- **Pluralize correctly**: `1 file`, `3 files`.

## Detail pane (right)

1. **Stage stepper**: `✓ Scanning 0:01 ──── ● Copying 0:32 ──── ○ Finishing ──── ○ Done`.
   Done stages show `✓` green + their duration. Current stage shows `●` orange + its running time.
   Stalled stage shows `◐` yellow + `stalled 1:12`. Future stages show `○` dim.
2. **Stall banner** (only when stalled; see Stall detection). Yellow border, 3 lines:
   - `⚠ No progress for 1:12 · robocopy is waiting to retry a locked file` (yellow, bold)
   - What and why: `DEMO\dbs\demo1043.dbx is in use by another process (error 32). Retry 3 in 0:18. Everything else is copied.`
   - Actions: `s skip this file   R retry now   c cancel job   o open log`
3. **Progress**: one wide bar (fill the pane width minus 6 cols for the percent), bold percent
   to its right. Below it: `306.12 MB of 700.49 MB` left, `ETA 0:37` right (stalled: `ETA waiting on a locked file`, yellow).
4. **THROUGHPUT** (left) | **STATUS** (right):
   - Throughput: 4-row block chart of the last 64 one-second samples, 1 cell per sample,
     y-axis `0` to a nice max (round up the peak), axis lines `│` and `─` in the border color,
     caption `−64 s … MB/s, last 64 s … now`. Rule text: `10.6 MB/s now · 9.6 MB/s avg`.
   - Status (label 10 cols dim, value bold, note dim):
     `Speed`, `Files done / total`, `Elapsed` (+ `started HH:MM:SS`), `Retries` (+ next retry
     time and interval), `Errors` (`none` green, else count red + short reason).
5. **PATHS**: `From`, `To` (full paths, wrap rather than truncate), `Log` (cyan) + `o open`.
6. **ACTIVITY**: the last ~5 events, columns: time (10 cols dim) · kind (9 cols, colored) · message.
   Kinds: `scan` green, `start`/`copy` blue, `retry n` yellow, `error` red, `skip` yellow, `done` green.

## Rendering details

- **Progress bars** use eighth blocks for smooth fill: full cells `█` in the status color, one
  partial cell from `▏▎▍▌▋▊▉` (status-color foreground on the track background), and the
  track as `█` in `#292e42`.
- **Chart**: for each sample `v`, `e = round(v / max * rows * 8)`; for row `r` (0 = bottom),
  the cell is `█` if `e − r*8 ≥ 8`, `" ▁▂▃▄▅▆▇"[e − r*8]` if > 0, else space.
  Bar color = the job's status color (orange, or yellow when stalled), so the drop to zero is obvious.
  Before the job started, samples are empty (blank cells), not zero bars.
- Status colors: running `#ff9e64`, done `#9ece6a`, stalled/warn `#e0af68`, error `#f7768e`,
  info `#7aa2f7`, dim `#828bb8`, card bg `#1f2335`, track/rules `#292e42`, borders `#3b4261`.

## Footer

- Line 1: `JOBS` badge · `1 running` (orange) or `1 stalled` (yellow) · `3 done` (green) ·
  dim `reports stay in %LOCALAPPDATA%\sentier\jobs\ until dismissed`. Right: `⇅ 1 job · 44%`,
  or `⚠ 1 job stalled · 99%` in yellow. Show the right-hand summary in the main explorer status
  bar too, so a stall is visible when the panel is closed.
- Line 2 (the only hint row): `j k pick job · c cancel · r retry failed · o log · ↵ dismiss · J close`.
  Add `s skip locked file` when the selected job is stalled. Remove the old in-pane hint row.

## Data the job engine needs to provide

Per job, kept in the job's state (and persisted to the job report so finished jobs can show it):

- `stage` + a timestamp for each stage transition (for the stepper durations).
- `bytes_done`, `bytes_total`, `files_done`, `files_total`, `current_file`.
- **Speed samples**: ring buffer of 64 one-second byte-delta samples. `avg = bytes_done / elapsed`.
  ETA from a smoothed speed (EWMA over ~10 s), not the instantaneous one.
- `retries`, `next_retry_at`, retry interval, `errors: Vec<{path, code, message, at}>`.
- `last_progress_at`: when `bytes_done` last increased.
- **Event log**: structured events (`scan`, `start`, `copy <file>`, `error`, `retry n`, `skip`,
  `done`) written to the job log *and* kept as an in-memory tail (last ~50) for ACTIVITY.
  For robocopy, parse its output/log lines into these events: per-file lines, `ERROR nn (0x…)`
  lines with the Windows error code + message, and `Waiting 30 seconds... Retrying...` lines.

### Stall detection

- A job is **stalled** when `now − last_progress_at > 10 s` while stage is Copying or Finishing.
- The banner explains the most specific cause available, in this order:
  1. Last event is an error + a retry is pending → "waiting to retry `<file>`: `<message>` (error N). Retry k in m:ss."
  2. Stage is Finishing → "finishing: `<what>`" (e.g. verifying, setting attributes, waiting for robocopy to exit).
  3. Otherwise → "no bytes moved; the engine is still running (PID n)". If the process is gone →
     treat as FAILED.
- Clear the stall state as soon as bytes move again.

### New actions

- `s` — skip the current (locked/failing) file: record it as skipped, continue the job.
  For robocopy this likely means stopping the process and restarting with the file excluded
  (`/XF`), or handling the retry loop in sentier with `/R:0` so sentier owns retries.
- `R` (inside the jobs panel) — retry now: skip the remaining wait.
- A job that finishes with skipped files lands in FINISHED with a yellow `!` and `N skipped`.
- Check that `s` and `R` don't clash with existing bindings in the jobs panel context;
  pick alternatives if they do and update the help overlay.

## Out of scope

- Bandwidth limits, queue reordering. (Pause / resume was added: `p`.)
