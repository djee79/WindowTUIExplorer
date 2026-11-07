# Help overlay redesign (`g?`) — implementation spec

Redesign of sentier's key-help overlay. A visual mockup exists (`help-overlay-mockup.dc.html`,
an HTML reference — open it for intent only; ignore its `<x-dc>`/`sc-for` template syntax).
Everything below is buildable on a character grid; no pixel-only effects.

## Goals

1. Group bindings by task instead of one long flat list.
2. Never truncate. Wrap long descriptions with a hanging indent under the description column.
3. Make keys scannable: keys render as "chips", alternates side by side, special keys as glyphs.
4. Commands (`:` command line) get their own column, each showing its key equivalent, which
   replaces the "(also Space f f)" suffixes.
5. Type-to-filter across keys and commands.
6. Drive everything from the real keymap (default + `%APPDATA%\sentier\keymap.toml` remaps),
   so the overlay can never drift from actual bindings.

## Layout (wide terminal, ≥ ~200 cols)

```
 C:\Users\jdoe\tests\                                                     2026-11-06 10:50
╭─ help · 117 keys, 31 commands ────────────────────────────────────────────── sentier ─╮
│ ┌──────────────────────────────────┐        ␣ leader (Space)  C- Ctrl  A- Alt  ↵ Enter │
│ │ / type to filter keys and commands│                                                  │
│ └──────────────────────────────────┘                                                   │
│ MOVE ──────────────────────  SELECT & EDIT ─────────────  GO TO ──────────────  │ : GO & CONNECT ──────────────── │
│  j  ↓       down              Tab        select + down     gl  ␣l    Locations  │ :cd <path>   go to a folder  g␣ │
│  ...                          ...                          ...                  │ ...                             │
╰─ Esc or any key closes · / filter · C-n C-p move ──── remap in %APPDATA%\sentier\keymap.toml ─╯
```

- Status line (cwd + clock) stays visible above the overlay.
- Overlay: rounded border (`╭╮╰╯`), 1-cell outer margin, 3-col inner horizontal padding.
- Top border: left title `help · {N} keys, {M} commands` (counts computed); right `sentier`.
- Bottom border: left hints; right `remap in %APPDATA%\sentier\keymap.toml` (path in green).
- Row 1 inside: filter input (left, ~60 cols, bordered) and the notation legend (right).
- Body: 4 columns. Columns 1–3 are key bindings, **46 cols** wide each. Column 4 takes the rest,
  separated by a vertical rule `│` with 3 cols of padding. Gap between columns: 4 cols.
- Key column inside a binding row: **14 cols**, description = remainder.
- Command row: command+args **25 cols**, description flexible, key hint **6 cols** right-aligned.
- One blank line between sections.
- Section header: `TITLE ` in header color, bold, followed by `─` to the column edge in a rule color.

### Responsive fallbacks

- Width < 200: 3 columns (bindings in 2 cols, commands in col 3), then 2, then 1.
- Height too small: the body scrolls (`C-n`/`C-p`, `j`/`k` while filter is not focused), and
  the column flow is recalculated, not clipped.
- When packing sections into columns, balance column heights (greedy by line count, sections
  never split across columns).

## Rendering rules

- **Key chip:** ` key ` = 1 space of padding on each side, chip background, key foreground, bold.
  Alternates are separated by a single plain space.
- **Notation:** `<Down>`→`↓`, `<Up>`→`↑`, `<Left>`→`←`, `<Right>`→`→`, `<CR>`→`↵`, `<BS>`→`⌫`,
  `<C-x>`→`C-x`, `<A-CR>`→`A-↵`, `<Home>`/`<End>`/`<PageDown>`/`<PageUp>`→`Home`/`End`/`PgDn`/`PgUp`,
  `<char>`→`‹c›`.
- **Leader:** render the leader as `␣` in the leader color (cyan), followed by the rest of the
  sequence in the key color: `␣ff`. The legend says `␣ leader (Space)`. If the user remaps the
  leader, show its own glyph or name instead.
- **Descriptions:** text in a trailing `( … )` is dimmed (e.g. `top` + dim `(or line n)`).
- **Commands:** command name purple bold, args (`<path>`, `[name]`) cyan italic, hint key dim.

## Palette (Tokyo Night)

| Role | Hex |
|---|---|
| Background | `#1a1b26` |
| Input background | `#16161e` |
| Chip background | `#24283b` |
| Section rule / input border | `#292e42` |
| Overlay border | `#3b4261` |
| Text | `#c0caf5` |
| Description | `#a9b1d6` |
| Dim / notes (≥4.5:1 on bg, don't go darker) | `#828bb8` |
| Key | `#ff9e64` |
| Leader glyph, command args | `#7dcfff` |
| Section headers (keys) | `#7aa2f7` |
| Commands, command section headers | `#bb9af7` |
| Path in footer | `#9ece6a` |

Make key color and header color configurable (theme).

## Data model

Each action in the keymap needs: `id`, `group`, `desc` (optional `(note)` suffix), bound keys,
and optionally a `command` (`:picker files`) plus `args`. The overlay is generated from that:

- Binding sections: group actions by `group`, in a fixed group order (below).
- Command sections: every action with a `command`, grouped by `cmd_group`; the hint column
  shows that action's shortest key binding (if any).
- Counts in the title come from the same data. Filter matches key text, desc and command name;
  sections with no matches are hidden, matches highlighted.

## Grouping and content

Columns 1–3 (key bindings):

**MOVE** — `j ↓` down · `k ↑` up · `h ← ⌫` parent directory · `l → ↵` open (enter dir / run file) ·
`gg Home` top (or line n) · `G End` bottom (or line n) · `C-d` half page down · `C-u` half page up ·
`C-f PgDn` page down · `C-b PgUp` page up · `zz zt zb` cursor line to middle / top / bottom
**HISTORY** — `H C-o` back in history · `L` forward in history · `''` previous directory
**SEARCH & FILTER** — `/` search forward · `?` search backward · `n N` next / previous match ·
`f` filter this directory · `s` search names below here · `S` search contents below here ·
`gr` go to the result's folder · `,s` cycle results sort · `Esc` clear filter / search
**PREVIEW** — `zp` full-screen preview · `]p [p` next / previous PDF page · `gP` go to PDF page ·
`]m [m` next / previous match in the preview

**SELECT & EDIT** — `Tab` select + down · `v V` visual (range select) · `yy` yank (copy) · `x` cut ·
`p` paste here · `P` paste, replacing (asks) · `.` repeat the last paste here ·
`a` new file (end in \ for a folder) · `A` new folder · `r cw` rename (Locations: a pin) ·
`u` undo the last bulk rename here · `dd` delete to the Recycle Bin (Locations: …) ·
`D` delete permanently (type yes)
**PATHS & WINDOWS** — `yp` copy path · `yP` copy UNC path (mapped drives resolved) · `yn` copy name ·
`O` open with… (menu) · `gR` reveal in Explorer · `gx` this folder in Explorer ·
`A-↵` Windows Properties · `gw` new WezTerm tab here · `␣e go` this folder in the editor (Neovim)
**PINS & TAGS** — `'‹c›` jump to a pin · `m‹c›` pin this folder under a key · `M` pin the item under
the cursor · `T` edit a pin's tags (Locations) · `␣p` pins picker · `␣t` tags (pick, filter, rename) ·
`␣nu` new web app pin (a URL)

**GO TO** — `gl ␣l` Locations · `gm` Machines (user PCs) · `gd` Downloads · `~` home ·
`g\` root of this drive or share · `g Space` a path (any form, UNC too) ·
`g.` toggle hidden / system files · `g?` this help
**PICKERS** — `␣␣` launcher (apps, web apps, pins) · `␣ff` find files below here · `␣fr` recent
folders · `␣/` grep file contents below here · `␣:` commands (every action, searchable) ·
`␣sk` search the key bindings · `␣m` machines picker
**SYNC & BACKUP** — `␣j J` jobs panel · `␣ns` new sync preset from this folder · `␣nb` new backup
preset from this folder · `␣rs` run a sync preset · `␣rb` run a backup preset ·
`␣ni` import wtm's launcher entries
**SESSION** — `R` retry / refresh / re-probe · `:` command line · `q` quit · `ZZ Q` quit and cd the
shell here · `ZQ` quit without writing the cwd · `C-c` quit at once ·
`Esc` in a prompt: back to normal · `Esc Esc` in a prompt: cancel

Column 4 (commands):

**: GO & CONNECT** — `:cd <path>` · `:connect [path]` · `:net list` · `:net drop <server>` · `:machines`
**: PINS & TAGS** — `:pin` · `:pin url [URL]` · `:import-wtm` · `:tags` · `:tag rename <old> <new>` ·
`:tag delete <tag>` · `:view save <name>`
**: PICKERS** — `:launcher` · `:picker files` · `:picker dirs` · `:picker grep` · `:picker commands`
**: SYNC & BACKUP** — `:sync [name]` · `:sync! [name]` · `:sync new` · `:backup [name]` · `:backup new`
**: RENAME & OPTIONS** — `:rename` · `:rename-edit` · `:undo-rename [n]` · `:set rnu|nornu|nu|nonu` · `:q`

Use the existing descriptions from the code for these. **The old overlay truncated some descriptions,
so the mockup guessed their endings. Use the real text from the code**, not the mockup, for:
`dd`, `:connect`, `:pin url`, `:import-wtm`, `:sync`, `:sync!`, `:view save`.

## Out of scope / notes

- `C-n`/`C-p` to scroll the overlay is a proposed new binding; add it only if it doesn't clash.
- Esc/any key still closes; `/` focuses the filter instead of closing.
- Keep the overlay generation in its own module so `␣sk` (search the key bindings) can reuse
  the same data and filter.
