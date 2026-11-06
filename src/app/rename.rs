//! The bulk rename panel (CLAUDE.md section 16, docs/screens/12-bulk-rename.txt).
//!
//! `r` / `cw` with a selection or a visual range, or `:rename`, opens it on those items
//! (on the cursor item when nothing is selected). Two fields, Search for and Replace
//! with, and a live preview table recomputed on every key.
//!
//! - In a field (insert): type; `Tab` / `S-Tab` switch fields; `Esc` to the options.
//! - Options (normal): `r` regex, `b` boost, `a` match all, `c` case, `n` apply to,
//!   `f` files, `d` folders, `t` text format, `e` enumerate, `x` random, `m` date source,
//!   `o` only changed; `j` / `k` `gg` `G` `^d` `^u` move in the table; `Space` excludes /
//!   includes the item under the cursor; `i` / `Tab` back to the fields; `q` / `Esc` close.
//! - `Enter` (either mode): rename after one question, unless something is blocked.

use super::keys::PromptKind;
use super::tags::Confirm;
use super::{App, Mode};
use crate::fs::EntryKind;
use crate::fs::ops::FileOp;
use crate::prompt::{Edit, LineEditor};
use crate::rename::engine::Options;
use crate::rename::plan::{self, Existing, Plan, Source, Status};
use crate::rename::tokens::Names;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Search,
    Replace,
}

pub struct RenamePanel {
    pub sources: Vec<Source>,
    pub search: LineEditor,
    pub replace: LineEditor,
    pub field: Field,
    /// In the options / table (normal) rather than typing in a field.
    pub normal: bool,
    pub options: Options,
    /// The preview, or why there is none (a bad regex).
    pub plan: Result<Plan, String>,
    /// Row in the table (among the rows shown).
    pub cursor: usize,
    pub offset: usize,
    /// The folder, for the title.
    pub dir: String,
    existing: Existing,
    seed: u64,
    pending_g: bool,
}

impl RenamePanel {
    /// The rows shown: all, or only those that change (`o`).
    pub fn shown(&self) -> Vec<usize> {
        let Ok(p) = &self.plan else {
            return Vec::new();
        };
        (0..p.rows.len())
            .filter(|&i| {
                !self.options.only_changed
                    || matches!(
                        p.rows[i].status,
                        Status::Rename | Status::Warn(_) | Status::Blocked(_)
                    )
            })
            .collect()
    }

    fn recompute(&mut self, names: &Names) {
        self.plan = plan::plan(
            &self.sources,
            &self.search.text(),
            &self.replace.text(),
            &self.options,
            names,
            &self.existing,
            self.seed,
        );
        let n = self.shown().len();
        self.cursor = self.cursor.min(n.saturating_sub(1));
    }
}

impl App {
    /// `r` / `cw` with a selection, `:rename`: the panel on the selection, the visual
    /// range, or the cursor item.
    pub(super) fn open_rename_panel(&mut self) {
        let Some(listing) = self.view_listing.clone() else {
            return;
        };
        let Some(dir) = self.cwd.as_fs().map(|p| p.display()) else {
            return self.set_status("bulk rename works inside a folder".into(), false);
        };
        let idx = if self.visual.is_some() || !self.marked.is_empty() {
            self.operand(1)
        } else {
            self.view.get(self.cursor).copied().into_iter().collect()
        };
        self.visual = None;
        let sources: Vec<Source> = idx
            .iter()
            .filter_map(|&i| listing.entries.get(i))
            .filter(|e| matches!(e.kind, EntryKind::File | EntryKind::Dir))
            .map(|e| Source {
                dir: dir.clone(),
                name: e.name.clone(),
                is_dir: e.kind == EntryKind::Dir,
                created: e.created,
                modified: e.modified,
                ..Source::default()
            })
            .collect();
        if sources.is_empty() {
            return self.set_status("nothing to rename here".into(), false);
        }
        let mut existing = Existing::new();
        existing.insert(
            dir.to_lowercase(),
            listing
                .entries
                .iter()
                .map(|e| e.name.to_lowercase())
                .collect::<HashSet<_>>(),
        );
        let mut panel = RenamePanel {
            sources,
            search: LineEditor::new(""),
            replace: LineEditor::new(""),
            field: Field::Search,
            normal: false,
            options: self.rename_options,
            plan: Ok(Plan::default()),
            cursor: 0,
            offset: 0,
            dir,
            existing,
            seed: crate::frecency::now_secs(),
            pending_g: false,
        };
        panel.recompute(&self.rename_names);
        self.rename_panel = Some(panel);
        self.mode = Mode::Rename;
        self.dirty = true;
    }

    pub(super) fn on_rename_key(&mut self, ev: KeyEvent) {
        let names = self.rename_names.clone();
        let Some(p) = self.rename_panel.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        if ev.code == KeyCode::Enter {
            return self.confirm_rename();
        }
        if !p.normal {
            match ev.code {
                KeyCode::Esc => p.normal = true,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
                    p.field = match p.field {
                        Field::Search => Field::Replace,
                        Field::Replace => Field::Search,
                    };
                }
                _ => {
                    let editor = match p.field {
                        Field::Search => &mut p.search,
                        Field::Replace => &mut p.replace,
                    };
                    if let Edit::Changed = editor.handle(&ev, "") {
                        p.recompute(&names);
                    }
                }
            }
            self.dirty = true;
            return;
        }
        let rows = p.shown().len();
        let last = rows.saturating_sub(1);
        let g = std::mem::take(&mut p.pending_g);
        let o = &mut p.options;
        let mut changed = true;
        let mut note: Option<&str> = None;
        match ev.code {
            KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
                self.close_rename_panel();
                return self.set_status("nothing renamed".into(), false);
            }
            KeyCode::Char('i' | 'I' | 'A') | KeyCode::Tab if !ctrl => {
                p.normal = false;
                changed = false;
            }
            KeyCode::Char('r') if !ctrl => o.regex = !o.regex,
            KeyCode::Char('b') if !ctrl => {
                o.boost = !o.boost;
                if o.boost {
                    o.regex = true;
                }
            }
            KeyCode::Char('a') if !ctrl => o.match_all = !o.match_all,
            KeyCode::Char('c') if !ctrl => o.case = !o.case,
            KeyCode::Char('n') if !ctrl => o.apply_to = o.apply_to.next(),
            KeyCode::Char('f') if !ctrl => o.files = !o.files,
            KeyCode::Char('d') if ctrl => {
                p.cursor = (p.cursor + 10).min(last);
                changed = false;
            }
            KeyCode::Char('u') if ctrl => {
                p.cursor = p.cursor.saturating_sub(10);
                changed = false;
            }
            KeyCode::Char('d') => o.folders = !o.folders,
            KeyCode::Char('s') if !ctrl => {
                o.subfolders = !o.subfolders;
                note = Some("subfolders: walking into them comes with the next update");
            }
            KeyCode::Char('t') => o.format = o.format.next(),
            KeyCode::Char('e') => o.enumerate = !o.enumerate,
            KeyCode::Char('x') => o.random = !o.random,
            KeyCode::Char('m') => o.dates = o.dates.next(),
            KeyCode::Char('o') => {
                o.only_changed = !o.only_changed;
                p.cursor = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                p.cursor = (p.cursor + 1).min(last);
                changed = false;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                p.cursor = p.cursor.saturating_sub(1);
                changed = false;
            }
            KeyCode::Char('G') => {
                p.cursor = last;
                changed = false;
            }
            KeyCode::Char('g') if g => {
                p.cursor = 0;
                changed = false;
            }
            KeyCode::Char('g') => {
                p.pending_g = true;
                changed = false;
            }
            // Space: leave the item under the cursor out (or back in), then move down.
            KeyCode::Char(' ') => {
                let shown = p.shown();
                if let (Ok(plan), Some(&row)) = (&p.plan, shown.get(p.cursor))
                    && let Some(r) = plan.rows.get(row)
                    && let Some(s) = p.sources.get_mut(r.source)
                {
                    s.excluded = !s.excluded;
                }
                p.cursor = (p.cursor + 1).min(last);
            }
            _ => changed = false,
        }
        if changed {
            p.recompute(&names);
            // Remembered for the next time the panel opens (PowerRename restores flags).
            self.rename_options = p.options;
        }
        if let Some(n) = note {
            self.set_status(n.into(), false);
        }
        self.dirty = true;
    }

    /// Enter: one question, then the rename (two-phase, so swaps and case-only work).
    fn confirm_rename(&mut self) {
        let Some(p) = self.rename_panel.as_ref() else {
            return;
        };
        let plan = match &p.plan {
            Ok(plan) => plan,
            Err(e) => return self.set_status(format!("can't rename: {e}"), true),
        };
        if plan.blocked > 0 {
            return self.set_status(
                format!(
                    "{} blocked: fix them or exclude them (Space) first",
                    plan.blocked
                ),
                true,
            );
        }
        let pairs = plan.pairs(&p.sources);
        if pairs.is_empty() {
            return self.set_status("nothing would change".into(), false);
        }
        let skipped = plan.unchanged + plan.excluded;
        let admin = self.admin_prefix().unwrap_or_default();
        let what = match pairs.len() {
            1 => "1 item".to_string(),
            n => format!("{n} items"),
        };
        let why = match skipped {
            0 => format!("{admin}Rename {what}?"),
            n => format!("{admin}Rename {what}? ({n} skipped)"),
        };
        self.confirm = Some(Confirm::FileOp {
            op: FileOp::BulkRename(pairs),
            why,
        });
        self.open_prompt_with(PromptKind::Confirm, "");
    }

    pub(super) fn close_rename_panel(&mut self) {
        self.rename_panel = None;
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    /// After the question: yes closes the panel (the rename runs); no comes back to it.
    pub(super) fn rename_answered(&mut self, yes: bool) {
        if self.rename_panel.is_none() {
            return;
        }
        if yes {
            self.rename_panel = None;
        } else {
            self.mode = Mode::Rename;
        }
    }

    pub(super) fn on_locale_names(&mut self, names: Names) {
        self.rename_names = names;
        let names = self.rename_names.clone();
        if let Some(p) = self.rename_panel.as_mut() {
            p.recompute(&names);
        }
    }
}
