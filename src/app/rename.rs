//! The bulk rename panel (CLAUDE.md section 16, docs/screens/12-bulk-rename.txt).
//!
//! `r` / `cw` with a selection or a visual range, or `:rename`, opens it on those items
//! (on the cursor item when nothing is selected). Two fields, Search for and Replace
//! with, and a live preview table recomputed on every key.
//!
//! - In a field (insert): type; `Tab` / `S-Tab` switch fields; `^p` / `^n` step through
//!   the field's history (last 50); `Esc` to the options.
//! - Options (normal): `r` regex, `b` boost, `a` match all, `c` case, `n` apply to,
//!   `f` files, `d` folders, `s` subfolders (walks into the selected folders), `t` text
//!   format, `e` enumerate, `x` random, `m` date source, `o` only changed; `j` / `k` `gg`
//!   `G` `^d` `^u` move in the table; `Space` excludes / includes the item under the
//!   cursor; `i` / `Tab` back to the fields; `q` / `Esc` close.
//! - Either mode: `Enter` renames after one question (unless something is blocked);
//!   `^s` saves a preset (asks for a name); `^o` lists the presets.
//! - Photo / PDF values are read from the files (off-thread) once the Replace text or
//!   the date source needs them.
//!
//! Every applied batch is journaled; `u` / `:undo-rename [n]` rename batches back.

use super::keys::PromptKind;
use super::tags::Confirm;
use super::{App, Mode};
use crate::fs::EntryKind;
use crate::fs::ops::{FileOp, Undo};
use crate::prompt::{Edit, LineEditor};
use crate::rename::apply::Entry as JournalEntry;
use crate::rename::engine::{DateSource, Options};
use crate::rename::meta::Meta;
use crate::rename::plan::{self, Existing, Plan, Source, Status};
use crate::rename::presets::{self, Preset, State};
use crate::rename::tokens::{META_TOKENS, Names};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Search,
    Replace,
}

/// The `^o` list: its cursor, and whether `x` was pressed once (press again to delete).
pub struct PresetList {
    pub cursor: usize,
    pub delete_armed: bool,
}

pub struct RenamePanel {
    /// The items in the table: the selection, with the subfolders' contents after each
    /// folder when `s` is on.
    pub sources: Vec<Source>,
    /// The selection itself.
    base: Vec<Source>,
    /// The subfolders' contents, once walked.
    walked: Option<Vec<Source>>,
    pub walking: bool,
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
    /// Photo / PDF values read so far (by lowercased path); asked for once needed.
    metas: HashMap<String, Meta>,
    meta_asked: bool,
    pub meta_reading: bool,
    /// `^p` / `^n`: position in the active field's history, and what was typed before.
    hist: Option<usize>,
    draft: String,
    /// `^s`: the preset name being typed.
    pub naming: Option<LineEditor>,
    /// `^o`: the preset list.
    pub presets: Option<PresetList>,
    /// Paths left out with Space (they survive a rebuild of the sources).
    excluded: HashSet<String>,
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

    /// The selection, plus each folder's walked contents right after it when `s` is on;
    /// with what has been read and what was left out.
    fn rebuild(&mut self) {
        let mut out = Vec::new();
        for b in &self.base {
            out.push(b.clone());
            if !self.options.subfolders || !b.is_dir {
                continue;
            }
            if let Some(walked) = &self.walked {
                let root = b.path().to_lowercase();
                let under = format!("{root}\\");
                out.extend(
                    walked
                        .iter()
                        .filter(|w| {
                            let d = w.dir.to_lowercase();
                            d == root || d.starts_with(&under)
                        })
                        .cloned(),
                );
            }
        }
        for s in &mut out {
            let key = s.path().to_lowercase();
            s.excluded = self.excluded.contains(&key);
            if let Some(m) = self.metas.get(&key) {
                s.meta = Some(m.values.clone()).filter(|v| !v.is_empty());
                s.taken = m.taken;
                s.pdf_title = m.pdf_title.clone();
            }
        }
        self.sources = out;
    }

    /// Do the tokens or the date source need values from inside the files?
    fn needs_meta(&self) -> bool {
        let r = self.replace.text();
        self.options.dates == DateSource::Taken
            || r.contains("pdf_title")
            || META_TOKENS.iter().any(|t| r.contains(&format!("${t}")))
    }

    fn editor(&mut self) -> &mut LineEditor {
        match self.field {
            Field::Search => &mut self.search,
            Field::Replace => &mut self.replace,
        }
    }

    /// `^p` (older, `back`) / `^n` (newer) through the active field's history.
    fn step_history(&mut self, history: &State, back: bool) -> bool {
        let list = match self.field {
            Field::Search => &history.search,
            Field::Replace => &history.replace,
        };
        if list.is_empty() {
            return false;
        }
        let next = match (self.hist, back) {
            (None, true) => Some(0),
            (None, false) => return false,
            (Some(i), true) => Some((i + 1).min(list.len() - 1)),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
        };
        if self.hist.is_none() {
            self.draft = self.editor().text();
        }
        self.hist = next;
        let text = match next {
            Some(i) => list[i].clone(),
            None => std::mem::take(&mut self.draft),
        };
        *self.editor() = LineEditor::new(&text);
        true
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
        let base: Vec<Source> = idx
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
        if base.is_empty() {
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
        self.rename_gen += 1;
        let mut panel = RenamePanel {
            sources: base.clone(),
            base,
            walked: None,
            walking: false,
            search: LineEditor::new(""),
            replace: LineEditor::new(""),
            field: Field::Search,
            normal: false,
            options: self.rename_state.options,
            plan: Ok(Plan::default()),
            cursor: 0,
            offset: 0,
            dir,
            existing,
            seed: crate::frecency::now_secs(),
            pending_g: false,
            metas: HashMap::new(),
            meta_asked: false,
            meta_reading: false,
            hist: None,
            draft: String::new(),
            naming: None,
            presets: None,
            excluded: HashSet::new(),
        };
        panel.rebuild();
        panel.recompute(&self.rename_names);
        self.rename_panel = Some(panel);
        self.mode = Mode::Rename;
        self.dirty = true;
        self.rename_fetch();
    }

    /// Starts what the panel now needs from disk: the subfolder walk, the file values.
    fn rename_fetch(&mut self) {
        let generation = self.rename_gen;
        let hidden = self.show_hidden;
        let Some(p) = self.rename_panel.as_mut() else {
            return;
        };
        if p.options.subfolders && p.walked.is_none() && !p.walking {
            let dirs: Vec<String> = p
                .base
                .iter()
                .filter(|s| s.is_dir)
                .map(Source::path)
                .collect();
            if !dirs.is_empty() {
                p.walking = true;
                self.worker.rename_walk(generation, dirs, hidden);
            }
        }
        if !p.meta_asked && p.needs_meta() {
            p.meta_asked = true;
            let files: Vec<String> = p
                .sources
                .iter()
                .filter(|s| !s.is_dir)
                .map(Source::path)
                .collect();
            if files.iter().any(|f| crate::rename::meta::wanted(f)) {
                p.meta_reading = true;
                self.worker.rename_meta(generation, files);
            }
        }
    }

    pub(super) fn on_rename_walk(
        &mut self,
        generation: u64,
        items: Vec<Source>,
        existing: Existing,
        capped: bool,
    ) {
        if generation != self.rename_gen {
            return;
        }
        let names = self.rename_names.clone();
        let Some(p) = self.rename_panel.as_mut() else {
            return;
        };
        p.walking = false;
        let n = items.len();
        // Their files may need reading too, if the first read already went.
        let files: Vec<String> = items
            .iter()
            .filter(|s| !s.is_dir && crate::rename::meta::wanted(&s.name))
            .map(Source::path)
            .collect();
        p.walked = Some(items);
        p.existing.extend(existing);
        p.rebuild();
        p.recompute(&names);
        if p.meta_asked && !files.is_empty() {
            p.meta_reading = true;
            self.worker.rename_meta(generation, files);
        }
        let msg = if capped {
            format!("subfolders: stopped at {n} items")
        } else {
            format!("subfolders: {n} items inside")
        };
        self.set_status(msg, capped);
        self.dirty = true;
    }

    pub(super) fn on_rename_meta(
        &mut self,
        generation: u64,
        metas: Vec<(String, Meta)>,
        done: bool,
    ) {
        if generation != self.rename_gen {
            return;
        }
        let names = self.rename_names.clone();
        let Some(p) = self.rename_panel.as_mut() else {
            return;
        };
        for (path, m) in metas {
            p.metas.insert(path.to_lowercase(), m);
        }
        if done {
            p.meta_reading = false;
        }
        p.rebuild();
        p.recompute(&names);
        self.dirty = true;
    }

    /// rename.toml and the history, read at startup. Merged, not replaced: anything done
    /// before the read finished (a preset saved, a rename made) stays.
    pub(super) fn on_rename_store(&mut self, presets: Result<Vec<Preset>, String>, state: State) {
        match presets {
            Ok(list) => {
                for p in list {
                    if !self
                        .rename_presets
                        .iter()
                        .any(|q| q.name.eq_ignore_ascii_case(&p.name))
                    {
                        presets::upsert(&mut self.rename_presets, p);
                    }
                }
            }
            Err(e) => self.set_status(e, true),
        }
        let touched = !self.rename_state.search.is_empty()
            || !self.rename_state.replace.is_empty()
            || self.rename_state.options != Options::default();
        if !touched {
            self.rename_state = state;
            return;
        }
        // This session's entries first (newest), then the saved ones.
        for (mine, saved) in [
            (&mut self.rename_state.search, state.search),
            (&mut self.rename_state.replace, state.replace),
        ] {
            for s in saved {
                if !mine.contains(&s) && mine.len() < presets::HISTORY {
                    mine.push(s);
                }
            }
        }
    }

    pub(super) fn on_rename_key(&mut self, ev: KeyEvent) {
        let names = self.rename_names.clone();
        let Some(p) = self.rename_panel.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        self.dirty = true;
        if p.naming.is_some() {
            return self.on_preset_name_key(ev);
        }
        if p.presets.is_some() {
            return self.on_preset_list_key(ev);
        }
        match ev.code {
            KeyCode::Enter => return self.confirm_rename(),
            KeyCode::Char('s') if ctrl => {
                p.naming = Some(LineEditor::new(""));
                return;
            }
            KeyCode::Char('o') if ctrl => {
                if self.rename_presets.is_empty() {
                    return self.set_status(
                        "no presets yet: ^s saves the current search, replace and options".into(),
                        false,
                    );
                }
                p.presets = Some(PresetList {
                    cursor: 0,
                    delete_armed: false,
                });
                return;
            }
            _ => {}
        }
        if !p.normal {
            match ev.code {
                KeyCode::Esc => p.normal = true,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
                    p.field = match p.field {
                        Field::Search => Field::Replace,
                        Field::Replace => Field::Search,
                    };
                    p.hist = None;
                }
                KeyCode::Char(c @ ('p' | 'n')) if ctrl => {
                    if p.step_history(&self.rename_state, c == 'p') {
                        p.recompute(&names);
                    }
                }
                _ => {
                    if let Edit::Changed = p.editor().handle(&ev, "") {
                        p.hist = None;
                        p.recompute(&names);
                    }
                }
            }
            return self.rename_fetch();
        }
        let rows = p.shown().len();
        let last = rows.saturating_sub(1);
        let g = std::mem::take(&mut p.pending_g);
        let o = &mut p.options;
        let mut changed = true;
        let mut note = None;
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
            KeyCode::Char('s') => {
                o.subfolders = !o.subfolders;
                if o.subfolders && !p.base.iter().any(|s| s.is_dir) {
                    note = Some("subfolders: no folder in the selection to walk into");
                }
                p.rebuild();
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
                    && let Some(s) = p.sources.get(r.source)
                {
                    let key = s.path().to_lowercase();
                    if !p.excluded.remove(&key) {
                        p.excluded.insert(key);
                    }
                }
                p.rebuild();
                p.cursor = (p.cursor + 1).min(last);
            }
            _ => changed = false,
        }
        if changed {
            p.recompute(&names);
            // Remembered for the next time the panel opens (PowerRename restores flags).
            self.rename_state.options = p.options;
            self.rename_fetch();
        }
        if let Some(n) = note {
            self.set_status(n.into(), false);
        }
    }

    /// `^s`: typing the preset's name. Enter saves (same name replaces), Esc drops it.
    fn on_preset_name_key(&mut self, ev: KeyEvent) {
        let Some(p) = self.rename_panel.as_mut() else {
            return;
        };
        let Some(ed) = p.naming.as_mut() else {
            return;
        };
        match ev.code {
            KeyCode::Esc => p.naming = None,
            KeyCode::Enter => {
                let name = ed.text().trim().to_string();
                if name.is_empty() {
                    return self.set_status("a name for the preset, please".into(), false);
                }
                p.naming = None;
                let preset = Preset {
                    name: name.clone(),
                    search: p.search.text(),
                    replace: p.replace.text(),
                    options: p.options,
                };
                let replaced = self
                    .rename_presets
                    .iter()
                    .any(|q| q.name.eq_ignore_ascii_case(&name));
                presets::upsert(&mut self.rename_presets, preset);
                self.worker.save_rename_presets(self.rename_presets.clone());
                self.set_status(
                    format!(
                        "{} preset {name}  (^o lists them)",
                        if replaced { "updated" } else { "saved" }
                    ),
                    false,
                );
            }
            _ => {
                ed.handle(&ev, "");
            }
        }
    }

    /// `^o`: `j` / `k`, Enter loads, `x` twice deletes, Esc / q closes.
    fn on_preset_list_key(&mut self, ev: KeyEvent) {
        let names = self.rename_names.clone();
        let n = self.rename_presets.len();
        let Some(p) = self.rename_panel.as_mut() else {
            return;
        };
        let Some(l) = p.presets.as_mut() else {
            return;
        };
        let armed = std::mem::take(&mut l.delete_armed);
        match ev.code {
            KeyCode::Esc | KeyCode::Char('q') => p.presets = None,
            KeyCode::Char('j') | KeyCode::Down => {
                l.cursor = (l.cursor + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => l.cursor = l.cursor.saturating_sub(1),
            KeyCode::Char('x') | KeyCode::Delete if !armed => {
                l.delete_armed = true;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if l.cursor < n {
                    let gone = self.rename_presets.remove(l.cursor);
                    l.cursor = l.cursor.min(n.saturating_sub(2));
                    if self.rename_presets.is_empty() {
                        p.presets = None;
                    }
                    self.worker.save_rename_presets(self.rename_presets.clone());
                    self.set_status(format!("deleted preset {}", gone.name), false);
                }
            }
            KeyCode::Enter => {
                let Some(pr) = self.rename_presets.get(l.cursor).cloned() else {
                    return;
                };
                p.presets = None;
                p.search = LineEditor::new(&pr.search);
                p.replace = LineEditor::new(&pr.replace);
                p.options = pr.options;
                p.hist = None;
                p.rebuild();
                p.recompute(&names);
                self.rename_state.options = pr.options;
                self.set_status(format!("preset {}", pr.name), false);
                self.rename_fetch();
            }
            _ => {}
        }
    }

    /// Enter: one question, then the rename (two-phase, so swaps and case-only work).
    fn confirm_rename(&mut self) {
        let Some(p) = self.rename_panel.as_ref() else {
            return;
        };
        if p.walking {
            return self.set_status("still walking the subfolders…".into(), false);
        }
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
        self.worker.save_rename_state(self.rename_state.clone());
        self.dirty = true;
    }

    /// After the question: yes closes the panel (the rename runs) and remembers the
    /// search and replace; no comes back to it.
    pub(super) fn rename_answered(&mut self, yes: bool) {
        let Some(p) = self.rename_panel.as_ref() else {
            return;
        };
        if yes {
            presets::remember(&mut self.rename_state.search, &p.search.text());
            presets::remember(&mut self.rename_state.replace, &p.replace.text());
            self.rename_state.options = p.options;
            self.rename_panel = None;
            self.worker.save_rename_state(self.rename_state.clone());
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

    // ---------------------------------------------------------------- undo

    /// `u` (`here`: the newest batch in this folder or below) / `:undo-rename [n]` (the
    /// n newest anywhere): the journal is read first, then one question.
    pub(super) fn undo_rename(&mut self, n: usize, here: bool) {
        let under = if here {
            match self.cwd.as_fs() {
                Some(p) => Some(p.display()),
                None => {
                    return self.set_status(
                        "u undoes a bulk rename in a folder; :undo-rename works anywhere".into(),
                        false,
                    );
                }
            }
        } else {
            None
        };
        self.worker.rename_journal(n.max(1), under);
    }

    pub(super) fn on_rename_journal(
        &mut self,
        want: usize,
        here: bool,
        batches: Vec<(JournalEntry, Vec<bool>)>,
    ) {
        if batches.is_empty() {
            let msg = if here {
                "no bulk rename to undo in this folder (:undo-rename undoes the last one anywhere)"
            } else {
                "no bulk rename to undo"
            };
            return self.set_status(msg.into(), false);
        }
        let mut undos = Vec::new();
        let mut gone = 0;
        for (e, here) in &batches {
            let pairs: Vec<(String, String)> = e
                .batch
                .undo_pairs()
                .into_iter()
                .zip(here)
                .filter_map(|(pair, &h)| if h { Some(pair) } else { None })
                .collect();
            gone += here.iter().filter(|h| !**h).count();
            undos.push(Undo {
                id: e.id.clone(),
                batch: e.batch.clone(),
                pairs,
            });
        }
        let items: usize = undos.iter().map(|u| u.pairs.len()).sum();
        if items == 0 {
            return self.set_status(
                "the renamed files aren't there any more: nothing to undo".into(),
                true,
            );
        }
        let when = batches
            .first()
            .map(|(e, _)| when(e.batch.time))
            .unwrap_or_default();
        let what = if items == 1 {
            "1 item".to_string()
        } else {
            format!("{items} items")
        };
        let mut why = if batches.len() == 1 {
            format!("Undo the last bulk rename ({what}, {when})?")
        } else {
            format!("Undo the last {} bulk renames ({what})?", batches.len())
        };
        if batches.len() < want {
            why.push_str(&format!(" (only {} in the journal)", batches.len()));
        }
        if gone > 0 {
            why.push_str(&format!(" ({gone} no longer there, skipped)"));
        }
        let admin = self.admin_prefix().unwrap_or_default();
        self.confirm = Some(Confirm::FileOp {
            op: FileOp::UndoRename(undos),
            why: format!("{admin}{why}"),
        });
        self.open_prompt_with(PromptKind::Confirm, "");
    }
}

/// `today 10:42`, `yesterday 18:05`, `2026-11-02 09:10`.
fn when(secs: u64) -> String {
    use chrono::TimeZone;
    let Some(t) = chrono::Local.timestamp_opt(secs as i64, 0).single() else {
        return String::new();
    };
    let today = chrono::Local::now().date_naive();
    let day = t.date_naive();
    if day == today {
        t.format("today %H:%M").to_string()
    } else if today.pred_opt() == Some(day) {
        t.format("yesterday %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}
