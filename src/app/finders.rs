//! The finder pickers (CLAUDE.md section 7):
//!
//! - `<leader>ff` files below the cwd: the search walker (`type:f`; `depth:4` on a share,
//!   `^l` lifts it) streams them in while you type; `matched / scanned` with a spinner.
//! - `<leader>fr` recent folders: every folder visited, by frecency, then the cwd's
//!   subfolders.
//! - `<leader>/` grep: the text is a content search (literal, `re:`, `ext:`…), walked
//!   again 250 ms after the last key; rows are files with their match count.
//! - `<leader>:` every action and `:` command; `<leader>sk` every key binding. Enter runs
//!   it (a command that takes an argument opens `:` with it typed).
//!
//! On file / folder rows: Enter opens (first opener / enter the folder), `^o` the
//! open-with menu, `^v` reveal in Explorer, `^y` copy the path, `^a` pin it, `Tab`
//! chooses several, then Enter yanks them (`p` pastes them as a copy).

use super::open::OpenTarget;
use super::{App, Mode, PromptKind};
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::CancelToken;
use crate::jobs::{Item, Op};
use crate::keymap::Action;
use crate::picker::{ItemKind, PickItem, Picker, Source};
use crate::search::SearchMatcher;
use crate::search::query::Query;
use crate::search::walk::Hit;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Grep waits this long after the last key before walking again.
const GREP_DEBOUNCE: Duration = Duration::from_millis(250);
/// The files picker's depth on a network share, until `^l`.
const NETWORK_DEPTH: usize = 4;

/// A walk feeding the open picker.
pub struct PickerWalk {
    id: u64,
    root: FsPath,
    cancel: CancelToken,
}

/// A file or folder row's target.
#[derive(Debug, Clone)]
pub struct PickedPath {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

fn row(id: usize, name: String, detail: String, kind: ItemKind, score: u64) -> PickItem {
    PickItem {
        id,
        name,
        detail,
        kind,
        key: None,
        tags: Vec::new(),
        unc: false,
        local: false,
        reachable: None,
        server: None,
        score,
    }
}

impl App {
    fn finder_root(&mut self) -> Option<FsPath> {
        let root = self.cwd.real().as_fs().cloned();
        if root.is_none() {
            self.set_status("this works inside a folder (not Locations)".into(), false);
        }
        root
    }

    fn stop_picker_walk(&mut self) {
        if let Some(w) = self.picker_walk.take() {
            w.cancel.cancel();
        }
    }

    fn start_picker_walk(
        &mut self,
        root: FsPath,
        query: &str,
        content: bool,
    ) -> Result<(), String> {
        self.stop_picker_walk();
        let q = Query::parse(query).map_err(|e| e.to_string())?;
        let matcher = Arc::new(SearchMatcher::new(&q, content)?);
        let id = self.next_search_id;
        self.next_search_id += 1;
        let cancel = self.worker.search(id, &root, matcher);
        self.picker_walk = Some(PickerWalk { id, root, cancel });
        if let Some(p) = self.picker.as_mut() {
            p.walking = true;
        }
        Ok(())
    }

    /// `<leader>ff` (`lift`: `^l`, no depth limit on a share).
    pub(super) fn open_files_picker(&mut self, lift: bool) {
        let Some(root) = self.finder_root() else {
            return;
        };
        let limited = root.is_network() && !lift;
        let query = if limited {
            format!("type:f depth:{NETWORK_DEPTH}")
        } else {
            "type:f".to_string()
        };
        self.picker_paths.clear();
        let mut picker = Picker::new("Files", Source::Files, Vec::new(), self.worker.waker(), "");
        picker.note = Some(if limited {
            format!(
                "network walk, depth {NETWORK_DEPTH} · ^l lifts the limit · in {}",
                root.display()
            )
        } else {
            format!("in {}", root.display())
        });
        self.show_picker(picker);
        if let Err(e) = self.start_picker_walk(root, &query, false) {
            self.set_status(format!("files: {e}"), true);
        }
    }

    /// `<leader>fr`: visited folders by frecency, then this folder's subfolders.
    pub(super) fn open_dirs_picker(&mut self) {
        let now = crate::frecency::now_secs();
        let mut paths: Vec<(String, u64)> = self.usage.dirs(now);
        if let (Some(here), Some(listing)) = (self.cwd.as_fs(), self.view_listing.as_ref()) {
            for e in listing.entries.iter().filter(|e| e.is_container()) {
                if let Ok(p) = here.join(&e.name) {
                    let d = p.display();
                    if !paths.iter().any(|(x, _)| x.eq_ignore_ascii_case(&d)) {
                        paths.push((d, 0));
                    }
                }
            }
        }
        self.picker_paths = paths
            .iter()
            .map(|(p, _)| PickedPath {
                path: p.clone(),
                is_dir: true,
                size: 0,
            })
            .collect();
        let items = paths
            .into_iter()
            .enumerate()
            .map(|(i, (p, score))| row(i, p, String::new(), ItemKind::Dir, score))
            .collect();
        let mut picker = Picker::new(
            "Recent folders",
            Source::Dirs,
            items,
            self.worker.waker(),
            "",
        );
        picker.note = Some("every folder you open counts · most used first".into());
        self.show_picker(picker);
    }

    /// `<leader>/`: contents below the cwd, searched as you type.
    pub(super) fn open_grep_picker(&mut self) {
        let Some(root) = self.finder_root() else {
            return;
        };
        self.picker_paths.clear();
        self.stop_picker_walk();
        let mut picker = Picker::new("Grep", Source::Grep, Vec::new(), self.worker.waker(), "");
        picker.note = Some(format!(
            "type to search contents (literal; re: for a regex; ext:pml) · in {}",
            root.display()
        ));
        self.grep_root = Some(root);
        self.show_picker(picker);
    }

    /// `<leader>:` (actions and `:` commands) or `<leader>sk` (key bindings only).
    pub(super) fn open_commands_picker(&mut self, keys_only: bool) {
        self.picker_actions = Vec::new();
        let mut items = Vec::new();
        // The same rows as `g?` (crate::help): its notation (`␣ff`), its sections.
        let help = crate::help::HelpData::build(&self.keymap);
        for s in &help.sections {
            let crate::help::Rows::Keys(rows) = &s.rows else {
                continue;
            };
            for r in rows {
                // The fixed keys (C-c, prompt Esc) aren't actions to run.
                let Some(action) = r.action.clone() else {
                    continue;
                };
                let keys: Vec<String> = r.chips.iter().map(crate::help::Chip::plain).collect();
                let desc = match &r.note {
                    Some(n) => format!("{} {n}", r.desc),
                    None => r.desc.clone(),
                };
                items.push(row(
                    self.picker_actions.len(),
                    desc,
                    format!("{}   · {}", keys.join(" "), s.title.to_lowercase()),
                    ItemKind::Binding,
                    0,
                ));
                self.picker_actions.push(action);
            }
        }
        if !keys_only {
            for (i, c) in crate::keymap::COMMANDS.iter().enumerate() {
                items.push(row(
                    i,
                    c.desc.to_string(),
                    format!(":{}", c.line()),
                    ItemKind::Command,
                    0,
                ));
            }
        }
        let (title, source) = if keys_only {
            ("Keymaps", Source::Keymaps)
        } else {
            ("Commands", Source::Commands)
        };
        let mut picker = Picker::new(title, source, items, self.worker.waker(), "");
        picker.note = Some("Enter runs it · remap keys in %APPDATA%\\sentier\\keymap.toml".into());
        self.show_picker(picker);
    }

    // -------------------------------------------------------------- walks

    /// A walk batch: true when it was the picker's (not a results folder's).
    pub(super) fn finders_progress(&mut self, id: u64, hits: &[Hit], scanned: u64) -> bool {
        let Some(w) = self.picker_walk.as_ref().filter(|w| w.id == id) else {
            return false;
        };
        let base = w.root.display();
        let grep = self
            .picker
            .as_ref()
            .is_some_and(|p| p.source == Source::Grep);
        let mut items = Vec::with_capacity(hits.len());
        for h in hits {
            let id = self.picker_paths.len();
            self.picker_paths.push(PickedPath {
                path: format!("{}\\{}", base.trim_end_matches('\\'), h.rel),
                is_dir: h.is_dir,
                size: h.size,
            });
            let detail = if grep {
                match h.score {
                    1 => "1 match".to_string(),
                    n => format!("{n} matches"),
                }
            } else {
                String::new()
            };
            let kind = if h.is_dir {
                ItemKind::Dir
            } else {
                ItemKind::File
            };
            items.push(row(id, h.rel.clone(), detail, kind, u64::from(h.score)));
        }
        if let Some(p) = self.picker.as_mut() {
            p.scanned = p.scanned.max(scanned);
            p.add(items);
        }
        self.dirty = true;
        true
    }

    pub(super) fn finders_done(&mut self, id: u64, scanned: u64) -> bool {
        if self.picker_walk.as_ref().is_none_or(|w| w.id != id) {
            return false;
        }
        if let Some(p) = self.picker.as_mut() {
            p.walking = false;
            p.scanned = p.scanned.max(scanned);
        }
        self.dirty = true;
        true
    }

    /// Grep: the text changed; walk again once typing pauses.
    pub(super) fn grep_text_changed(&mut self) {
        if self
            .picker
            .as_ref()
            .is_some_and(|p| p.source == Source::Grep)
        {
            self.grep_due = Some(Instant::now() + GREP_DEBOUNCE);
        }
    }

    pub(super) fn finders_tick(&mut self, now: Instant) {
        if self.picker.is_none() {
            // The picker closed: its walk has nobody to feed.
            if self.picker_walk.is_some() {
                self.stop_picker_walk();
            }
            self.grep_due = None;
            return;
        }
        if self.grep_due.is_none_or(|d| now < d) {
            return;
        }
        self.grep_due = None;
        let (Some(root), Some(text)) = (
            self.grep_root.clone(),
            self.picker.as_ref().map(|p| p.editor.text()),
        ) else {
            return;
        };
        self.picker_paths.clear();
        self.stop_picker_walk();
        if let Some(p) = self.picker.as_mut() {
            p.reset_items();
            p.walking = false;
        }
        if text.trim().is_empty() {
            return;
        }
        if let Err(e) = self.start_picker_walk(root, &text, true)
            && let Some(p) = self.picker.as_mut()
        {
            p.note = Some(format!("grep: {e}"));
        }
        self.dirty = true;
    }

    // -------------------------------------------------------------- actions

    fn picked(&self, id: usize) -> Option<(PickedPath, Loc)> {
        let p = self.picker_paths.get(id)?.clone();
        let loc = Loc::parse(&p.path).ok()?;
        Some((p, loc))
    }

    fn picked_target(&self, id: usize) -> Option<OpenTarget> {
        let (p, loc) = self.picked(id)?;
        Some(OpenTarget {
            name: loc.name(),
            path: p.path,
            loc,
            is_dir: p.is_dir,
        })
    }

    /// Enter on a file / folder row (or, with rows chosen by Tab, yank them all).
    pub(super) fn open_picked(&mut self, id: usize, chosen: Vec<usize>) {
        if !chosen.is_empty() {
            let items: Vec<Item> = chosen
                .iter()
                .filter_map(|&i| {
                    let (p, loc) = self.picked(i)?;
                    Some(Item {
                        path: loc.as_fs()?.clone(),
                        is_dir: p.is_dir,
                        size: p.size,
                    })
                })
                .collect();
            let n = items.len();
            self.register = Some(super::jobs::Register {
                op: Op::Copy,
                items,
            });
            return self.set_status(format!("yanked {n} · p pastes them into a folder"), false);
        }
        let Some(t) = self.picked_target(id) else {
            return;
        };
        if t.is_dir {
            self.navigate(t.loc, None);
        } else {
            // Into its folder with the cursor on it, then its first opener.
            if let Some(dir) = t.loc.parent() {
                self.navigate(dir, Some(t.name.clone()));
            }
            self.open_with_first(vec![t]);
        }
    }

    /// `^o` `^v` `^y` `^a` on a file / folder row. False when the row isn't one.
    pub(super) fn picked_action(&mut self, key: char) -> bool {
        let Some(item) = self.picker.as_ref().and_then(|p| p.selected()).cloned() else {
            return false;
        };
        if !matches!(item.kind, ItemKind::File | ItemKind::Dir) {
            return false;
        }
        let Some(t) = self.picked_target(item.id) else {
            return true;
        };
        self.close_picker();
        match key {
            'o' => self.open_menu_for(vec![t]),
            'v' => self.worker.reveal(t.path),
            'y' => self.run_opener("copy-path", vec![t], false),
            'a' => self.pin_loc(&t.loc, t.is_dir),
            _ => {}
        }
        true
    }

    /// Enter on a Binding / Command row.
    pub(super) fn run_picked_command(&mut self, kind: ItemKind, id: usize) {
        match kind {
            ItemKind::Binding => {
                if let Some(a) = self.picker_actions.get(id).cloned() {
                    self.run(a, None);
                }
            }
            ItemKind::Command => {
                let Some(&crate::keymap::Command {
                    name: cmd, args, ..
                }) = crate::keymap::COMMANDS.get(id)
                else {
                    return;
                };
                if args.is_empty() || args.starts_with('[') {
                    match Action::parse(cmd) {
                        Ok(a) => self.run(a, None),
                        Err(e) => self.set_status(e, true),
                    }
                } else {
                    // It needs an argument: the command line, ready for it.
                    self.open_prompt_with(PromptKind::Command, &format!("{cmd} "));
                }
            }
            _ => {}
        }
        if self.mode == Mode::Picker && self.picker.is_none() {
            self.mode = Mode::Normal;
        }
    }

    /// config.toml read before the TUI starts: the picture protocol, then the rest.
    pub fn apply_early_config(&mut self, cfg: crate::config::Config, problem: Option<String>) {
        let protocol = cfg
            .preview
            .protocol
            .clone()
            .filter(|p| !p.trim().eq_ignore_ascii_case("auto"));
        self.graphics = crate::preview::graphics_with(protocol.as_deref());
        self.config = cfg;
        self.apply_config();
        if let Some(p) = problem {
            self.set_status(p, true);
        }
    }

    /// `show_hidden`, `numbers`, `preview.max_network_mb` from config.toml.
    pub(super) fn apply_config(&mut self) {
        if let Some(h) = self.config.show_hidden
            && h != self.show_hidden
        {
            self.show_hidden = h;
            self.rebuild_view();
        }
        if let Some(n) = self.config.numbers.clone() {
            match crate::config::numbers(&n) {
                Ok((number, relative)) => {
                    self.number = number;
                    self.relative_number = relative;
                }
                Err(e) => self.set_status(e, true),
            }
        }
        if let Some(mb) = self.config.preview.max_network_mb {
            self.worker.set_preview_limit(mb);
        }
        self.dirty = true;
    }

    /// `go` / `<leader>e`: this folder in the editor (config `editor`, $VISUAL, $EDITOR,
    /// else nvim), in this terminal; the listing is refreshed after.
    pub(super) fn edit_here(&mut self) {
        let Some(dir) = self.cwd.real().as_fs().map(FsPath::display) else {
            return self.set_status("not a real folder here".into(), false);
        };
        let editor = self.config.editor();
        // `code --wait`-style editors come with their own arguments.
        let mut parts = crate::open::rules::split_args(&editor);
        if parts.is_empty() {
            return;
        }
        let program = parts.remove(0);
        parts.push(dir);
        self.inline = Some(super::fileops::Inline {
            program: program.clone(),
            args: parts,
            file: None,
            then: super::fileops::InlineThen::Refresh { what: program },
        });
    }

    /// pins.toml changed outside: take it, unless a pin is being edited or asked about
    /// (indices would shift under it); then it waits for that to end.
    pub(super) fn apply_pending_pins(&mut self) {
        if self.pins_pending.is_none() || self.pin_draft.is_some() || self.confirm.is_some() {
            return;
        }
        let Some(store) = self.pins_pending.take() else {
            return;
        };
        if store == self.pins {
            return; // our own save, read back
        }
        self.pins = store;
        self.reload_root();
        self.set_status("pins.toml changed: reloaded".into(), false);
    }

    /// Unsaved folder visits, for main to write before exiting.
    pub fn take_unsaved_usage(&mut self) -> Option<crate::frecency::Usage> {
        std::mem::take(&mut self.usage_dirty).then(|| self.usage.clone())
    }
}
