//! The `g?` overlay's content (docs/HELP_OVERLAY_SPEC.md), built from the live keymap
//! (defaults + keymap.toml) and the `:` command table, so it can't drift from the keys.
//!
//! - Binding sections follow [`Group`] (each action's section), rows in keymap order,
//!   aliases folded into their action's row as extra chips (`j ↓`). A few related actions
//!   share a row (`n N` next / previous match), `Alt-1…9` is one row.
//! - Command sections follow [`CmdGroup`]; each command shows the shortest key that does
//!   the same, from the live keymap.
//! - [`HelpData::filter`] (type-to-filter) and [`layout`] (columns by width, sections
//!   balanced, never split) are pure; `ui/help.rs` only draws. `<leader>sk` reuses the
//!   rows.

use crate::keymap::{
    ANY_CHAR, Action, COMMANDS, CmdGroup, FIXED, Group, Key, Keymap, NAMED_ACTIONS,
};
use ratatui::crossterm::event::KeyCode;

/// Width of the key column in a binding row.
pub const KEY_W: usize = 14;
/// Width of the command + args column in a command row, and of its key hint.
pub const CMD_W: usize = 25;
pub const HINT_W: usize = 6;
/// A binding column's width, the gap between binding columns, and what separates them
/// from the commands column (3 spaces, `│`, 3 spaces).
pub const COL_W: usize = 46;
pub const GAP: usize = 4;
pub const RULE: usize = 7;
const CMD_MIN: usize = 48;

/// One key sequence as a chip: ` ␣ff `, ` C-d `, ` g Space `.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// Starts with the leader (drawn as `␣` in the leader colour).
    pub leader: bool,
    pub text: String,
}

impl Chip {
    /// Cells, padding included.
    pub fn width(&self) -> usize {
        2 + usize::from(self.leader) + crate::ui::width(&self.text)
    }

    /// As typed into the filter: `␣ff`.
    pub fn plain(&self) -> String {
        if self.leader {
            format!("␣{}", self.text)
        } else {
            self.text.clone()
        }
    }
}

/// The chips of a row, side by side with a space between.
pub fn chips_width(chips: &[Chip]) -> usize {
    chips.iter().map(Chip::width).sum::<usize>() + chips.len().saturating_sub(1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub chips: Vec<Chip>,
    pub desc: String,
    /// A trailing `(…)` of the description, drawn dimmed.
    pub note: Option<String>,
    /// What it runs (`<leader>sk` runs it); None for the fixed keys.
    pub action: Option<Action>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdRow {
    pub name: String,
    pub args: String,
    pub desc: String,
    pub hint: Option<Chip>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rows {
    Keys(Vec<KeyRow>),
    Commands(Vec<CmdRow>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Rows,
}

impl Section {
    pub fn is_commands(&self) -> bool {
        matches!(self.rows, Rows::Commands(_))
    }

    fn len(&self) -> usize {
        match &self.rows {
            Rows::Keys(r) => r.len(),
            Rows::Commands(r) => r.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HelpData {
    pub sections: Vec<Section>,
    /// Key sequences and commands listed (the title's counts).
    pub keys: usize,
    pub commands: usize,
}

/// Actions that share one row, with the row's description (spec: `zz zt zb`).
const JOINED: &[(&[&str], &str)] = &[
    (
        &["scroll-middle", "scroll-top", "scroll-bottom"],
        "cursor line to middle / top / bottom",
    ),
    (&["search-next", "search-prev"], "next / previous match"),
    (&["next-page", "prev-page"], "next / previous PDF page"),
    (
        &["next-match", "prev-match"],
        "next / previous match in the preview",
    ),
    (
        &["tab-next", "tab-prev"],
        "next / previous tab ({n}gt: tab n)",
    ),
    (
        &["sort-name", "sort-name-reverse"],
        "by name, A → Z (,N reversed)",
    ),
    (
        &["sort-modified", "sort-modified-reverse"],
        "by date modified, newest first (,M oldest)",
    ),
    (
        &["sort-created", "sort-created-reverse"],
        "by date created, newest first (,C oldest)",
    ),
    (
        &["sort-size", "sort-size-reverse"],
        "by size, biggest first (,S smallest)",
    ),
    (
        &["sort-ext", "sort-ext-reverse"],
        "by extension (,E reversed)",
    ),
];

/// A key in the overlay's notation: `↓`, `↵`, `C-d`, `A-↵`, `PgDn`, `‹c›`.
fn token(k: &Key) -> String {
    let name = match k.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "↵".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "S-Tab".to_string(),
        KeyCode::Backspace => "⌫".to_string(),
        KeyCode::Delete => "Del".to_string(),
        KeyCode::Insert => "Ins".to_string(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Null => "‹c›".to_string(),
        other => format!("{other:?}"),
    };
    let shift = k.shift && !matches!(k.code, KeyCode::Char(_));
    format!(
        "{}{}{}{name}",
        if k.ctrl { "C-" } else { "" },
        if k.alt { "A-" } else { "" },
        if shift { "S-" } else { "" },
    )
}

/// A key sequence as a chip: the leader first (if it starts with it), named keys set
/// apart by a space (`g Space`), plain ones run together (`gg`, `'‹c›`).
pub fn chip(keys: &[Key], leader: Key) -> Chip {
    let (lead, rest) = match keys.split_first() {
        Some((first, rest)) if *first == leader && !rest.is_empty() => (true, rest),
        _ => (false, keys),
    };
    let mut text = String::new();
    let mut prev_named = false;
    for k in rest {
        // Inside a leader sequence, the leader again is its glyph: `␣␣`.
        let t = if lead && *k == leader {
            "␣".to_string()
        } else {
            token(k)
        };
        let named = t.chars().count() > 1 && *k != ANY_CHAR;
        if !text.is_empty() && (named || prev_named) {
            text.push(' ');
        }
        text.push_str(&t);
        prev_named = named;
    }
    // A lone leader key (unusual) still shows as its glyph.
    if lead && text.is_empty() {
        return Chip {
            leader: true,
            text: String::new(),
        };
    }
    Chip { leader: lead, text }
}

/// `top (or line n)` → (`top`, `(or line n)`).
pub fn split_note(desc: &str) -> (String, Option<String>) {
    if desc.ends_with(')')
        && let Some(i) = desc.rfind(" (")
        && desc[i + 2..].matches('(').count() == desc[i + 2..].matches(')').count() - 1
    {
        return (desc[..i].to_string(), Some(desc[i + 1..].to_string()));
    }
    (desc.to_string(), None)
}

fn action_named(id: &str) -> Option<Action> {
    NAMED_ACTIONS
        .iter()
        .find(|(n, _)| *n == id)
        .map(|(_, a)| a.clone())
}

impl HelpData {
    pub fn build(km: &Keymap) -> HelpData {
        // One entry per action, in keymap order: its chips and first description.
        let mut acts: Vec<(Action, Vec<Chip>, String)> = Vec::new();
        let mut tabs: Vec<Key> = Vec::new();
        for b in &km.normal {
            if let Action::GotoTab(_) = b.action
                && let [k] = b.keys.as_slice()
            {
                tabs.push(*k);
                continue;
            }
            let c = chip(&b.keys, km.leader);
            match acts.iter_mut().find(|(a, _, _)| *a == b.action) {
                Some((_, chips, desc)) => {
                    if !chips.contains(&c) {
                        chips.push(c);
                    }
                    if desc.is_empty() {
                        desc.clone_from(&b.desc);
                    }
                }
                None => acts.push((b.action.clone(), vec![c], b.desc.clone())),
            }
        }
        // `zz zt zb`, `n N`…: one row, at the first one's place.
        for (ids, desc) in JOINED {
            let wanted: Vec<Action> = ids.iter().filter_map(|id| action_named(id)).collect();
            let at: Vec<usize> = wanted
                .iter()
                .filter_map(|w| acts.iter().position(|(a, _, _)| a == w))
                .collect();
            if at.len() < 2 {
                continue;
            }
            let chips: Vec<Chip> = at.iter().flat_map(|&i| acts[i].1.clone()).collect();
            let first = at[0];
            acts[first].1 = chips;
            acts[first].2 = desc.to_string();
            let mut drop: Vec<usize> = at[1..].to_vec();
            drop.sort_unstable_by(|a, b| b.cmp(a));
            for i in drop {
                acts.remove(i);
            }
        }
        let mut keys = 0;
        let mut sections = Vec::new();
        for g in Group::ALL {
            let mut rows: Vec<KeyRow> = acts
                .iter()
                .filter(|(a, _, d)| a.group() == g && !d.is_empty())
                .map(|(a, chips, d)| {
                    let (desc, note) = split_note(d);
                    KeyRow {
                        chips: chips.clone(),
                        desc,
                        note,
                        action: Some(a.clone()),
                    }
                })
                .collect();
            if g == Group::Tabs && !tabs.is_empty() {
                rows.push(tab_row(&tabs, km.leader));
            }
            rows.extend(FIXED.iter().filter(|(_, _, fg)| *fg == g).map(|(k, d, _)| {
                KeyRow {
                    chips: k
                        .split(' ')
                        .map(|t| Chip {
                            leader: false,
                            text: t.to_string(),
                        })
                        .collect(),
                    desc: d.to_string(),
                    note: None,
                    action: None,
                }
            }));
            keys += rows.iter().map(|r| r.chips.len()).sum::<usize>();
            if !rows.is_empty() {
                sections.push(Section {
                    title: g.title(),
                    rows: Rows::Keys(rows),
                });
            }
        }
        keys += tabs.len().saturating_sub(1);
        for g in CmdGroup::ALL {
            let rows: Vec<CmdRow> = COMMANDS
                .iter()
                .filter(|c| c.group == g)
                .map(|c| CmdRow {
                    name: format!(":{}", c.name),
                    args: c.args.to_string(),
                    desc: c.desc.to_string(),
                    hint: c
                        .hint_action()
                        .and_then(|a| km.shortest(&a))
                        .map(|ks| chip(&ks, km.leader)),
                })
                .collect();
            if !rows.is_empty() {
                sections.push(Section {
                    title: g.title(),
                    rows: Rows::Commands(rows),
                });
            }
        }
        HelpData {
            sections,
            keys,
            commands: COMMANDS.len(),
        }
    }

    /// Only the rows whose keys, description or command match `q` (case-insensitive);
    /// sections left empty go.
    pub fn filter(&self, q: &str) -> HelpData {
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return self.clone();
        }
        let has = |s: &str| s.to_lowercase().contains(&q);
        let sections = self
            .sections
            .iter()
            .filter_map(|s| {
                let rows = match &s.rows {
                    Rows::Keys(r) => Rows::Keys(
                        r.iter()
                            .filter(|r| {
                                r.chips.iter().any(|c| has(&c.plain()) || has(&c.text))
                                    || has(&r.desc)
                                    || r.note.as_deref().is_some_and(has)
                            })
                            .cloned()
                            .collect(),
                    ),
                    Rows::Commands(r) => Rows::Commands(
                        r.iter()
                            .filter(|r| has(&r.name) || has(&r.args) || has(&r.desc))
                            .cloned()
                            .collect(),
                    ),
                };
                let s = Section {
                    title: s.title,
                    rows,
                };
                (s.len() > 0).then_some(s)
            })
            .collect();
        HelpData {
            sections,
            keys: self.keys,
            commands: self.commands,
        }
    }
}

/// `Alt-1` … `Alt-9` as one row: ` A-1…9 `.
fn tab_row(keys: &[Key], leader: Key) -> KeyRow {
    let digits: Vec<u32> = keys
        .iter()
        .filter_map(|k| match k.code {
            KeyCode::Char(c) if k.alt && !k.ctrl => c.to_digit(10),
            _ => None,
        })
        .collect();
    let chips = if digits.len() == keys.len() && digits.len() > 1 {
        let (lo, hi) = (
            digits.iter().min().copied().unwrap_or(1),
            digits.iter().max().copied().unwrap_or(9),
        );
        vec![Chip {
            leader: false,
            text: format!("A-{lo}…{hi}"),
        }]
    } else {
        keys.iter().map(|k| chip(&[*k], leader)).collect()
    };
    KeyRow {
        chips,
        desc: "go to tab n".to_string(),
        note: None,
        action: Some(Action::GotoTab(1)),
    }
}

/// Word-wraps `text` into lines of at most `width` cells (a long word is cut).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        loop {
            let need = if line.is_empty() {
                crate::ui::width(&word)
            } else {
                crate::ui::width(&line) + 1 + crate::ui::width(&word)
            };
            if need <= width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            // A word longer than the line: cut it.
            let cut: String = word.chars().take(width).collect();
            word = word.chars().skip(width).collect();
            lines.push(cut);
            if word.is_empty() {
                break;
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Where a key row's description starts, and its wrapped lines.
pub fn key_row_lines(r: &KeyRow, col_w: usize) -> (usize, Vec<String>) {
    let start = KEY_W.max(chips_width(&r.chips) + 1);
    let text = match &r.note {
        Some(n) => format!("{} {n}", r.desc),
        None => r.desc.clone(),
    };
    (start, wrap(&text, col_w.saturating_sub(start).max(8)))
}

/// Where a command row's description starts, and its wrapped lines (the hint sits at the
/// right of the first).
pub fn cmd_row_lines(r: &CmdRow, col_w: usize) -> (usize, Vec<String>) {
    let head = crate::ui::width(&r.name)
        + if r.args.is_empty() {
            0
        } else {
            1 + crate::ui::width(&r.args)
        };
    let start = CMD_W.max(head + 1);
    let hint = r
        .hint
        .as_ref()
        .map_or(0, |h| crate::ui::width(&h.plain()))
        .max(HINT_W);
    let avail = col_w.saturating_sub(start + hint + 1).max(8);
    (start, wrap(&r.desc, avail))
}

fn section_height(s: &Section, col_w: usize) -> usize {
    1 + match &s.rows {
        Rows::Keys(r) => r
            .iter()
            .map(|r| key_row_lines(r, col_w).1.len())
            .sum::<usize>(),
        Rows::Commands(r) => r.iter().map(|r| cmd_row_lines(r, col_w).1.len()).sum(),
    }
}

/// One column of the body: its width and the sections in it (indices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub width: usize,
    pub sections: Vec<usize>,
    /// The commands column (drawn after the `│` rule).
    pub commands: bool,
}

/// The columns for an inner width of `w` cells: three binding columns and the commands
/// beside them when it fits (spec: ≥ 200 wide), then two, then one, then everything in a
/// single column. Sections fill the binding columns in order, heights balanced.
pub fn layout(data: &HelpData, w: usize) -> Vec<Column> {
    let keys: Vec<usize> = (0..data.sections.len())
        .filter(|&i| !data.sections[i].is_commands())
        .collect();
    let cmds: Vec<usize> = (0..data.sections.len())
        .filter(|&i| data.sections[i].is_commands())
        .collect();
    for nb in (1..=3usize).rev() {
        let need = nb * COL_W + (nb - 1) * GAP + RULE + CMD_MIN;
        if w >= need && !cmds.is_empty() && !keys.is_empty() {
            let cmd_w = w - (nb * COL_W + (nb - 1) * GAP + RULE);
            let mut cols: Vec<Column> = balance(data, &keys, nb, COL_W)
                .into_iter()
                .map(|sections| Column {
                    width: COL_W,
                    sections,
                    commands: false,
                })
                .collect();
            cols.push(Column {
                width: cmd_w,
                sections: cmds,
                commands: true,
            });
            return cols;
        }
    }
    vec![Column {
        width: w.max(20),
        sections: (0..data.sections.len()).collect(),
        commands: false,
    }]
}

/// Splits `ids` (in order) into `n` runs with the tallest as short as possible.
fn balance(data: &HelpData, ids: &[usize], n: usize, col_w: usize) -> Vec<Vec<usize>> {
    let h: Vec<usize> = ids
        .iter()
        .map(|&i| section_height(&data.sections[i], col_w))
        .collect();
    let m = ids.len();
    if n <= 1 || m <= 1 {
        return vec![ids.to_vec()];
    }
    // Height of a run i..j: its sections plus a blank line between each.
    let run = |i: usize, j: usize| -> usize {
        if j <= i {
            0
        } else {
            h[i..j].iter().sum::<usize>() + (j - i - 1)
        }
    };
    // best[k][j]: the smallest tallest column putting the first j sections in k columns.
    let inf = usize::MAX;
    let mut best = vec![vec![inf; m + 1]; n + 1];
    let mut cut = vec![vec![0usize; m + 1]; n + 1];
    best[0][0] = 0;
    for k in 1..=n {
        for j in 0..=m {
            for i in 0..=j {
                if best[k - 1][i] == inf {
                    continue;
                }
                let v = best[k - 1][i].max(run(i, j));
                if v < best[k][j] {
                    best[k][j] = v;
                    cut[k][j] = i;
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut j = m;
    for k in (1..=n).rev() {
        let i = cut[k][j];
        out.push(ids[i..j].to_vec());
        j = i;
    }
    out.reverse();
    out.retain(|c| !c.is_empty());
    out
}

/// Lines a column takes (headers, rows, blanks between sections).
#[cfg(test)]
pub fn column_height(data: &HelpData, col: &Column) -> usize {
    let n = col.sections.len();
    col.sections
        .iter()
        .map(|&i| section_height(&data.sections[i], col.width))
        .sum::<usize>()
        + n.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> HelpData {
        HelpData::build(&Keymap::default())
    }

    fn row<'a>(d: &'a HelpData, desc: &str) -> Option<&'a KeyRow> {
        d.sections.iter().find_map(|s| match &s.rows {
            Rows::Keys(r) => r.iter().find(|r| r.desc == desc),
            Rows::Commands(_) => None,
        })
    }

    fn chips(r: &KeyRow) -> Vec<String> {
        r.chips.iter().map(Chip::plain).collect()
    }

    #[test]
    fn sections_follow_the_spec_and_the_keymap() {
        let d = data();
        let titles: Vec<&str> = d.sections.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            [
                "MOVE",
                "HISTORY",
                "TABS",
                "SEARCH & FILTER",
                "SORT",
                "PREVIEW",
                "SELECT & EDIT",
                "PATHS & WINDOWS",
                "PINS & TAGS",
                "GO TO",
                "PICKERS",
                "SYNC & BACKUP",
                "SESSION",
                ": GO & CONNECT",
                ": PINS & TAGS",
                ": PICKERS",
                ": SYNC & BACKUP",
                ": RENAME & OPTIONS",
            ]
        );
        // Aliases fold into chips, special keys become glyphs, the leader its own mark.
        let down = row(&d, "down").map(chips);
        assert_eq!(down, Some(vec!["j".to_string(), "↓".to_string()]));
        let top = row(&d, "top");
        assert_eq!(
            top.and_then(|r| r.note.clone()).as_deref(),
            Some("(or line n)")
        );
        assert_eq!(top.map(chips), Some(vec!["gg".into(), "Home".into()]));
        assert_eq!(
            row(&d, "find files below here").map(chips),
            Some(vec!["␣ff".to_string()])
        );
        assert_eq!(
            row(&d, "Windows Properties").map(chips),
            Some(vec!["A-↵".to_string()])
        );
        assert_eq!(
            row(&d, "go to a path").map(chips),
            Some(vec!["g Space".to_string()])
        );
        assert_eq!(
            row(&d, "jump to a pin").map(chips),
            Some(vec!["'‹c›".to_string()])
        );
        // Joined rows and the folded Alt-digits.
        assert_eq!(
            row(&d, "cursor line to middle / top / bottom").map(chips),
            Some(vec!["zz".into(), "zt".into(), "zb".into()])
        );
        assert_eq!(
            row(&d, "go to tab n").map(chips),
            Some(vec!["A-1…9".to_string()])
        );
        // Sorting: a key and its capital on one row, the reverse in the dimmed note.
        let size = row(&d, "by size, biggest first");
        assert_eq!(
            size.map(chips),
            Some(vec![",s".to_string(), ",S".to_string()])
        );
        assert_eq!(
            size.and_then(|r| r.note.clone()).as_deref(),
            Some("(,S smallest)")
        );
        assert!(
            row(&d, "next match").is_none(),
            "joined into next / previous"
        );
        // The fixed keys and the counts.
        assert!(row(&d, "quit at once").is_some());
        assert_eq!(d.commands, COMMANDS.len());
        assert!(d.keys > 100, "{}", d.keys);
    }

    #[test]
    fn commands_show_the_live_key_and_follow_remaps() {
        let hint = |d: &HelpData, name: &str| {
            d.sections.iter().find_map(|s| match &s.rows {
                Rows::Commands(r) => r
                    .iter()
                    .find(|r| r.name == name)
                    .map(|r| r.hint.as_ref().map(Chip::plain)),
                Rows::Keys(_) => None,
            })
        };
        let d = data();
        assert_eq!(hint(&d, ":picker files"), Some(Some("␣ff".to_string())));
        assert_eq!(hint(&d, ":cd"), Some(Some("g Space".to_string())));
        assert_eq!(hint(&d, ":tabnext"), Some(Some("gt".to_string())));
        assert_eq!(hint(&d, ":net list"), Some(None));
        // A keymap.toml remap: the shortest binding shows.
        let toml = "[normal]\nprepend = [ { on = \"<C-p>\", run = \"picker files\", desc = \"Find files\" } ]\n";
        let (km, problems) = crate::keymap::from_toml(toml);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            hint(&HelpData::build(&km), ":picker files"),
            Some(Some("C-p".to_string()))
        );
    }

    #[test]
    fn filter_keeps_matching_rows_only() {
        let d = data().filter("pdf");
        assert!(d.sections.iter().any(|s| s.title == "PREVIEW"));
        assert!(!d.sections.iter().any(|s| s.title == "MOVE"));
        let f = data().filter("␣ff");
        assert_eq!(f.sections.len(), 1);
        let c = data().filter("tag rename");
        assert!(c.sections.iter().any(|s| s.title == ": PINS & TAGS"));
        assert!(data().filter("zzzz").sections.is_empty());
        assert_eq!(data().filter("  ").sections.len(), data().sections.len());
    }

    #[test]
    fn notes_wraps_and_chips() {
        assert_eq!(
            split_note("delete to the Recycle Bin (Locations: remove)"),
            (
                "delete to the Recycle Bin".to_string(),
                Some("(Locations: remove)".to_string())
            )
        );
        assert_eq!(split_note("plain"), ("plain".to_string(), None));
        assert_eq!(split_note("a (b) c"), ("a (b) c".to_string(), None));
        assert_eq!(
            wrap("go to a folder any form", 10),
            ["go to a", "folder any", "form"]
        );
        assert_eq!(wrap("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
        assert_eq!(wrap("", 5), [""]);
        let c = Chip {
            leader: true,
            text: "ff".into(),
        };
        assert_eq!(c.width(), 5, "padding, ␣, ff");
    }

    #[test]
    fn layout_by_width_and_balanced() {
        let d = data();
        let four = layout(&d, 220);
        assert_eq!(four.len(), 4);
        assert!(four[3].commands && four[3].width >= CMD_MIN);
        assert_eq!(
            four[..3].iter().map(|c| c.width).collect::<Vec<_>>(),
            [COL_W; 3]
        );
        // Every binding section exactly once, in order.
        let order: Vec<usize> = four[..3].iter().flat_map(|c| c.sections.clone()).collect();
        assert_eq!(order, (0..13).collect::<Vec<_>>());
        // Balanced: no column more than the tallest section taller than another.
        let hs: Vec<usize> = four[..3].iter().map(|c| column_height(&d, c)).collect();
        let tallest = (0..13)
            .map(|i| section_height(&d.sections[i], COL_W))
            .max()
            .unwrap_or(0);
        let (lo, hi) = (hs.iter().min().copied(), hs.iter().max().copied());
        assert!(hi.unwrap_or(0) - lo.unwrap_or(0) <= tallest, "{hs:?}");
        assert_eq!(layout(&d, 160).len(), 3);
        assert_eq!(layout(&d, 110).len(), 2);
        let one = layout(&d, 80);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].sections.len(), d.sections.len());
    }
}
