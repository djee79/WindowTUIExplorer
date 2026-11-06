//! Tabs (Yazi's, with vim keys): `t` opens a tab on the current folder, `Alt-1…9` /
//! `{n}gt` go to tab n, `gt` / `gT` next / previous, `q` / `:q` closes a tab (quits on
//! the last one). `:tabnew [path]`, `:tabclose`, `:tabnext`, `:tabprevious` too.
//!
//! Each tab has its own folder, cursor, `H`/`L` history, `''`, selection and filter. The
//! listings cache, jobs, pins and search results are shared. Only the active tab's state
//! lives in the App's usual fields; the others wait in [`Tab`] snapshots.

use super::App;
use crate::fs::path::Loc;
use std::collections::HashSet;

/// The most tabs (what `Alt-1…9` can reach).
pub const MAX_TABS: usize = 9;

/// A tab that isn't shown: where it was and how it was left.
#[derive(Debug, Clone)]
pub struct Tab {
    pub cwd: Loc,
    cursor_name: Option<String>,
    offset: usize,
    back: Vec<Loc>,
    forward: Vec<Loc>,
    prev_dir: Option<Loc>,
    filter: String,
    marked: HashSet<String>,
}

impl Tab {
    pub fn new(cwd: Loc) -> Tab {
        Tab {
            cwd,
            cursor_name: None,
            offset: 0,
            back: Vec::new(),
            forward: Vec::new(),
            prev_dir: None,
            filter: String::new(),
            marked: HashSet::new(),
        }
    }

    /// Its label in the tab bar: the folder's name.
    pub fn label(&self) -> String {
        label_of(&self.cwd)
    }
}

/// `HIT`, `C:`, `aveva`, `Locations`, `search`.
pub fn label_of(loc: &Loc) -> String {
    let n = loc.name();
    if n.is_empty() { loc.display() } else { n }
}

impl App {
    /// The tab bar's entries: (number, label, active). Empty with a single tab.
    pub fn tab_labels(&self) -> Vec<(usize, String, bool)> {
        if self.tabs.len() < 2 {
            return Vec::new();
        }
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let active = i == self.tab;
                let label = if active {
                    label_of(&self.cwd)
                } else {
                    t.label()
                };
                (i + 1, label, active)
            })
            .collect()
    }

    /// The active tab's state, out of the App's fields.
    fn stash_tab(&mut self) -> Tab {
        self.remember_cursor();
        Tab {
            cwd: self.cwd.clone(),
            cursor_name: self.selected().map(|e| e.name.clone()),
            offset: self.offset,
            back: std::mem::take(&mut self.back),
            forward: std::mem::take(&mut self.forward),
            prev_dir: self.prev_dir.take(),
            filter: std::mem::take(&mut self.filter),
            marked: std::mem::take(&mut self.marked),
        }
    }

    /// Shows `t`: its folder (from the cache, refreshed), cursor, history, selection.
    fn show_tab(&mut self, t: Tab) {
        // Nothing of the old tab may leak in: no row is "selected" while switching.
        self.view_listing = None;
        self.view.clear();
        self.visual = None;
        self.cwd = t.cwd.clone();
        self.set_cwd(t.cwd, t.cursor_name);
        self.back = t.back;
        self.forward = t.forward;
        self.prev_dir = t.prev_dir;
        self.marked = t.marked;
        if !t.filter.is_empty() {
            self.filter = t.filter;
            self.rebuild_view();
        }
        self.offset = t.offset;
        self.dirty = true;
    }

    /// `t` / `:tabnew [path]`: a new tab right after this one, on this folder (or `path`).
    pub(super) fn new_tab(&mut self, path: Option<&str>) {
        if self.tabs.len() >= MAX_TABS {
            return self.set_status(format!("{MAX_TABS} tabs is the most (q closes one)"), false);
        }
        let here = self.stash_tab();
        let mut fresh = Tab::new(here.cwd.clone());
        fresh.cursor_name = here.cursor_name.clone();
        self.tabs[self.tab] = here;
        self.tab += 1;
        self.tabs.insert(self.tab, fresh.clone());
        self.show_tab(fresh);
        if let Some(p) = path.filter(|p| !p.trim().is_empty()) {
            self.cd(p);
        }
        self.tab_status();
    }

    /// `Alt-n` / `{n}gt` / `:tabnext n`: tab n (1-based).
    pub(super) fn goto_tab(&mut self, n: usize) {
        if n == 0 || n > self.tabs.len() {
            let have = self.tabs.len();
            return self.set_status(
                if have == 1 {
                    "only one tab (t opens another)".to_string()
                } else {
                    format!("no tab {n}: there are {have}")
                },
                false,
            );
        }
        let i = n - 1;
        if i == self.tab {
            return;
        }
        let here = self.stash_tab();
        self.tabs[self.tab] = here;
        self.tab = i;
        let t = self.tabs[i].clone();
        self.show_tab(t);
        self.tab_status();
    }

    /// `gt` / `gT`: the next / previous tab, wrapping.
    pub(super) fn cycle_tab(&mut self, steps: usize, forward: bool) {
        let len = self.tabs.len();
        if len < 2 {
            return self.set_status("only one tab (t opens another)".into(), false);
        }
        let s = steps % len;
        let i = if forward {
            (self.tab + s) % len
        } else {
            (self.tab + len - s) % len
        };
        self.goto_tab(i + 1);
    }

    /// `q` / `:q` / `:tabclose`: closes this tab; false when it's the last one (then `q`
    /// quits).
    pub(super) fn close_tab(&mut self) -> bool {
        if self.tabs.len() < 2 {
            return false;
        }
        self.remember_cursor();
        self.tabs.remove(self.tab);
        self.tab = self.tab.min(self.tabs.len() - 1);
        let t = self.tabs[self.tab].clone();
        self.show_tab(t);
        self.tab_status();
        true
    }

    fn tab_status(&mut self) {
        let msg = format!(
            "tab {} of {}  ·  Alt-1…9 / gt gT switch  ·  q closes it",
            self.tab + 1,
            self.tabs.len()
        );
        self.set_status(msg, false);
    }
}
