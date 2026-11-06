//! Open with (CLAUDE.md section 13).
//!
//! - `o` / `Enter` / `l` on a file run the first opener its rule offers (open.toml, or
//!   the defaults: the Windows default app). On a selection, all the selected files.
//! - `O` shows the menu: the rule's openers (the one used last for this extension first,
//!   marked `★ last`), then Open with… (Windows' dialog) and Properties. `1`–`9` pick,
//!   `j`/`k` (arrows, `^n`/`^p`) move, `Enter` runs, typing filters, `Esc` closes.
//! - `gR` reveal in Explorer, `Alt-Enter` Properties, `yp` / `yP` / `yn` copy the path /
//!   the UNC path / the name, `gx` Explorer here, `gw` a WezTerm tab here.
//!
//! Inline openers (Neovim) suspend the TUI and run in this terminal; detached ones are
//! started by the worker, their output in `logs\openers.log`.

use super::fileops::{Inline, InlineThen};
use super::{App, Mode};
use crate::fs::path::Loc;
use crate::open::rules::{self, RunMode, Target};
use crate::open::{self as open_menu, MenuItem};
use crate::pins::PinKind;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What `O` and `o` act on.
#[derive(Debug, Clone)]
pub struct OpenTarget {
    pub loc: Loc,
    pub path: String,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct OpenMenu {
    /// `B-1001_rev3.pdf`, `3 files`.
    pub title: String,
    /// The rule's pattern (`*.pdf`), for the section header.
    pub pattern: Option<String>,
    pub items: Vec<MenuItem>,
    pub filter: String,
    /// Index into the filtered rows.
    pub cursor: usize,
    targets: Vec<OpenTarget>,
    pub ext: String,
}

impl OpenMenu {
    /// Item indices shown, after the filter.
    pub fn shown(&self) -> Vec<usize> {
        open_menu::filter(&self.items, &self.filter)
    }
}

impl App {
    /// The selection (or the visual range), else the item under the cursor, as real
    /// paths. Virtual entries (Locations, machines) aren't openable here.
    pub(super) fn open_targets(&self) -> Vec<OpenTarget> {
        let Some(listing) = &self.view_listing else {
            return Vec::new();
        };
        let picked = if self.visual.is_some() || !self.marked.is_empty() {
            self.operand(1)
        } else {
            self.view.get(self.cursor).copied().into_iter().collect()
        };
        picked
            .into_iter()
            .filter_map(|i| {
                let e = listing.entries.get(i)?;
                let loc = self.cwd.join(&e.name).ok()?;
                let path = loc.as_fs()?.display();
                Some(OpenTarget {
                    name: loc.name(),
                    path,
                    loc,
                    is_dir: e.is_container(),
                })
            })
            .collect()
    }

    /// `o` on files: the first opener of the cursor file's rule, for every target.
    pub(super) fn open_with_first(&mut self, targets: Vec<OpenTarget>) {
        let Some(first) = targets.first() else {
            return self.set_status("nothing to open here".into(), false);
        };
        let ext = rules::ext_of(&first.name, first.is_dir);
        let id = open_menu::first_opener(
            &self.open_cfg,
            &first.name,
            first.is_dir,
            self.open_last.get(&ext).map(String::as_str),
        );
        self.run_opener(&id, targets, false);
    }

    fn menu_items(&self, name: &str, is_dir: bool, ext: &str) -> Vec<MenuItem> {
        open_menu::build_menu(
            &self.open_cfg,
            name,
            is_dir,
            self.open_last.get(ext).map(String::as_str),
            &self.open_resolved,
            self.open_registered.get(ext),
        )
    }

    /// Windows answered for `ext`: kept for the session, and the open menu (if it's for
    /// that extension) gets its "Windows suggests" rows, the cursor staying put.
    pub(super) fn on_registered(&mut self, ext: String, registered: open_menu::Registered) {
        self.open_registered.insert(ext.clone(), registered);
        let Some(m) = self.open_menu.as_ref() else {
            return;
        };
        if m.ext != ext {
            return;
        }
        let Some(first) = m.targets.first().cloned() else {
            return;
        };
        let at = m
            .shown()
            .get(m.cursor)
            .and_then(|&i| m.items.get(i))
            .map(|i| i.id.clone());
        let items = self.menu_items(&first.name, first.is_dir, &ext);
        if let Some(m) = self.open_menu.as_mut() {
            m.items = items;
            if let Some(id) = at {
                let shown = m.shown();
                m.cursor = shown
                    .iter()
                    .position(|&i| m.items[i].id == id)
                    .unwrap_or(m.cursor);
            }
        }
        self.dirty = true;
    }

    /// `O`: the menu for the cursor item (and the selection).
    pub(super) fn open_menu(&mut self) {
        let targets = self.open_targets();
        self.open_menu_for(targets);
    }

    /// The `O` menu for `targets` (the files pickers' `^o` passes their row).
    pub(super) fn open_menu_for(&mut self, targets: Vec<OpenTarget>) {
        let Some(first) = targets.first() else {
            return self.set_status("O opens files and folders".into(), false);
        };
        let ext = rules::ext_of(&first.name, first.is_dir);
        // What Windows has for this extension: asked once per session, filled in when
        // it arrives (the menu shows a "looking up" row until then).
        if !ext.is_empty() && !self.open_registered.contains_key(&ext) {
            self.worker.assoc_lookup(ext.clone());
        }
        let items = self.menu_items(&first.name, first.is_dir, &ext);
        let title = match targets.len() {
            1 => first.name.clone(),
            n => format!("{n} items"),
        };
        self.open_menu = Some(OpenMenu {
            title,
            pattern: self
                .open_cfg
                .pattern_for(&first.name, first.is_dir)
                .map(str::to_string),
            items,
            filter: String::new(),
            cursor: 0,
            targets,
            ext,
        });
        self.mode = Mode::OpenWith;
        self.dirty = true;
    }

    pub(super) fn on_open_menu_key(&mut self, ev: KeyEvent) {
        let Some(m) = self.open_menu.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let len = m.shown().len();
        match ev.code {
            KeyCode::Esc if !m.filter.is_empty() => {
                m.filter.clear();
                m.cursor = 0;
            }
            KeyCode::Esc => {
                self.open_menu = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Down => m.cursor = (m.cursor + 1).min(len.saturating_sub(1)),
            KeyCode::Up => m.cursor = m.cursor.saturating_sub(1),
            KeyCode::Char('n' | 'j') if ctrl => {
                m.cursor = (m.cursor + 1).min(len.saturating_sub(1))
            }
            KeyCode::Char('p' | 'k') if ctrl => m.cursor = m.cursor.saturating_sub(1),
            // j/k move until something is typed; then they're letters of the filter.
            KeyCode::Char('j') if m.filter.is_empty() => {
                m.cursor = (m.cursor + 1).min(len.saturating_sub(1))
            }
            KeyCode::Char('k') if m.filter.is_empty() => m.cursor = m.cursor.saturating_sub(1),
            KeyCode::Char(c @ '1'..='9') if m.filter.is_empty() => {
                let n = c as usize - '1' as usize;
                if n < len {
                    m.cursor = n;
                    self.menu_pick();
                }
            }
            KeyCode::Enter => self.menu_pick(),
            KeyCode::Backspace => {
                m.filter.pop();
                m.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                m.filter.push(c);
                m.cursor = 0;
            }
            _ => {}
        }
    }

    fn menu_pick(&mut self) {
        let Some(m) = self.open_menu.as_ref() else {
            return;
        };
        let shown = m.shown();
        let Some(item) = shown.get(m.cursor).and_then(|&i| m.items.get(i)).cloned() else {
            return;
        };
        if item.missing {
            return self.set_status(format!("{}: {}", item.label, item.detail), true);
        }
        let Some(m) = self.open_menu.take() else {
            return;
        };
        self.mode = Mode::Normal;
        // Remembered per extension (folders and extensionless files aren't).
        if self.open_cfg.remember && !m.ext.is_empty() {
            self.open_last.insert(m.ext.clone(), item.id.clone());
            self.worker.save_open_last(self.open_last.clone());
        }
        self.run_opener(&item.id, m.targets, true);
    }

    /// Runs opener `id` (built-in, configured or `pin:`) on `targets`.
    pub(super) fn run_opener(&mut self, id: &str, targets: Vec<OpenTarget>, from_menu: bool) {
        let Some(first) = targets.first().cloned() else {
            return;
        };
        let paths: Vec<String> = targets.iter().map(|t| t.path.clone()).collect();
        let folder = |t: &OpenTarget| {
            if t.is_dir {
                t.path.clone()
            } else {
                crate::jobs::parent_of(&t.path)
                    .unwrap_or(&t.path)
                    .to_string()
            }
        };
        let many = |what: &str| match paths.len() {
            1 => format!("{what} {}", first.name),
            n => format!("{what} {n} items"),
        };
        match id {
            "enter" => match targets.iter().find(|t| t.is_dir) {
                Some(t) => self.navigate(t.loc.clone(), None),
                None => self.set_status("enter: not a folder".into(), false),
            },
            "default" => {
                for t in targets {
                    self.worker.open(t.loc);
                }
                if !from_menu && paths.len() == 1 {
                    self.set_status(format!("opening {}…", first.name), false);
                }
            }
            "reveal" => self.worker.reveal(first.path.clone()),
            "explorer" => self.worker.run_detached(
                format!("Explorer: {}", folder(&first)),
                "explorer.exe".into(),
                vec![folder(&first)],
                None,
            ),
            "terminal-here" => {
                let dir = folder(&first);
                self.worker.run_detached(
                    format!("WezTerm tab in {dir}"),
                    "wezterm".into(),
                    vec!["cli".into(), "spawn".into(), "--cwd".into(), dir.clone()],
                    Some(dir),
                );
            }
            "copy-path" => self
                .worker
                .copy_text(many("copied the path of"), paths, false),
            "copy-unc" => self
                .worker
                .copy_text(many("copied the UNC path of"), paths, true),
            "openwith" => self.worker.open_with_dialog(first.path.clone()),
            "properties" => self.worker.properties(first.path.clone()),
            pin if pin.starts_with("pin:") => self.run_pin_opener(&pin[4..], &targets),
            assoc if assoc.starts_with("assoc:") => {
                let ext = rules::ext_of(&first.name, false);
                let app = self
                    .open_registered
                    .get(&ext)
                    .and_then(|r| r.apps.iter().find(|(n, _)| n == &assoc[6..]))
                    .map_or_else(|| "the app".to_string(), |(_, ui)| ui.clone());
                self.worker.assoc_invoke(
                    format!("{} with {app}", many("opened")),
                    ext,
                    assoc[6..].to_string(),
                    paths,
                );
            }
            name => self.run_configured(name, &targets),
        }
    }

    /// An `app` pin used as an opener: its command, its args, then the paths.
    fn run_pin_opener(&mut self, name: &str, targets: &[OpenTarget]) {
        let Some(pin) = self
            .pins
            .pins
            .iter()
            .find(|p| p.kind == PinKind::App && p.name.eq_ignore_ascii_case(name))
            .cloned()
        else {
            return self.set_status(format!("no app pin named {name:?}"), true);
        };
        let Some(command) = pin.command.clone() else {
            return;
        };
        let mut args = rules::split_args(pin.args.as_deref().unwrap_or(""));
        args.extend(targets.iter().map(|t| t.path.clone()));
        let dir = pin.dir.clone().or_else(|| {
            targets
                .first()
                .and_then(|t| crate::jobs::parent_of(&t.path).map(str::to_string))
        });
        self.worker
            .run_detached(format!("started {}", pin.name), command, args, dir);
    }

    fn run_configured(&mut self, name: &str, targets: &[OpenTarget]) {
        let Some(o) = self.open_cfg.openers.get(name).cloned() else {
            return self.set_status(format!("no opener named {name:?} (open.toml)"), true);
        };
        let program = self
            .open_resolved
            .get(name)
            .cloned()
            .flatten()
            .unwrap_or_else(|| o.run[0].clone());
        let ts: Vec<Target> = targets
            .iter()
            .map(|t| Target {
                path: t.path.clone(),
            })
            .collect();
        let lines = rules::command_lines(&o, &ts);
        let cwd = targets
            .first()
            .and_then(|t| crate::jobs::parent_of(&t.path).map(str::to_string));
        match o.mode {
            RunMode::Inline => {
                // One program at a time in this terminal: the first command line.
                let Some(line) = lines.into_iter().next() else {
                    return;
                };
                self.inline = Some(Inline {
                    program,
                    args: line.get(1..).unwrap_or_default().to_vec(),
                    file: None,
                    then: InlineThen::Refresh {
                        what: o.desc.clone(),
                    },
                });
            }
            RunMode::Detached => {
                for line in lines {
                    self.worker.run_detached(
                        format!("started {}", o.desc),
                        program.clone(),
                        line[1..].to_vec(),
                        cwd.clone(),
                    );
                }
            }
        }
    }

    /// The current folder, for `gx` / `gw`.
    fn here(&self) -> Option<OpenTarget> {
        let p = self.cwd.real().as_fs()?.clone();
        Some(OpenTarget {
            loc: Loc::Fs(p.clone()),
            path: p.display(),
            name: p.name(),
            is_dir: true,
        })
    }

    /// `gx` / `gw`: Explorer / a WezTerm tab in the current folder.
    pub(super) fn open_here(&mut self, id: &str) {
        match self.here() {
            Some(t) => self.run_opener(id, vec![t], false),
            None => self.set_status("not a real folder here".into(), false),
        }
    }

    /// `gR`, `Alt-Enter`, `yp`, `yP`: on the selection or the cursor item.
    pub(super) fn open_targets_with(&mut self, id: &str) {
        let targets = self.open_targets();
        if targets.is_empty() {
            return self.set_status("nothing here to act on".into(), false);
        }
        self.run_opener(id, targets, false);
    }

    /// `yn`: the names.
    pub(super) fn copy_names(&mut self) {
        let targets = self.open_targets();
        if targets.is_empty() {
            return self.set_status("nothing here to copy".into(), false);
        }
        let what = match targets.len() {
            1 => format!("copied the name {}", targets[0].name),
            n => format!("copied {n} names"),
        };
        let names = targets.into_iter().map(|t| t.name).collect();
        self.worker.copy_text(what, names, false);
    }

    pub(super) fn on_open_config(
        &mut self,
        cfg: rules::OpenConfig,
        problems: Vec<String>,
        resolved: std::collections::BTreeMap<String, Option<String>>,
        last: crate::open::run::LastUsed,
    ) {
        self.open_cfg = cfg;
        self.open_resolved = resolved;
        // Picks made before the file was read win.
        for (k, v) in last {
            self.open_last.entry(k).or_insert(v);
        }
        if let Some(p) = problems.first() {
            let more = match problems.len() {
                1 => String::new(),
                n => format!(" (+{} more in the log)", n - 1),
            };
            for p in &problems {
                tracing::warn!(problem = %p, "open.toml");
            }
            self.set_status(format!("{p}{more}"), true);
        }
    }

    pub(super) fn on_ran(&mut self, what: String, result: Result<(), String>) {
        match result {
            Ok(()) => self.set_status(what, false),
            Err(e) => self.set_status(format!("{what}: {e}"), true),
        }
    }
}
