//! The pickers in the app (CLAUDE.md sections 3 and 7): the pin picker (`<leader>p`:
//! pins and saved views) and the tag picker (`<leader>t`, `:tags`), and their keys.
//!
//! Insert mode (they open there) types into the query; `Esc` goes to normal mode, where
//! `j`/`k` (with counts), `gg`/`G`, `^d`/`^u`, `]t`/`[t` work, `i`/`a` go back to insert,
//! and `Esc`/`q` close. In both: `^j`/`^k` `^n`/`^p` move, `Enter` opens, `^e` edits,
//! `^t` edits tags, `^x` (normal mode: `D` / `dd`) deletes (asks first). Editing returns to the picker.
//! In the tag picker `Tab` chooses tags and `Enter` opens the pin picker filtered to
//! pins carrying any of them (`#a,b,c`; typing `#a #b` there narrows to all of them).

/// The pin-picker query for tags chosen in the tag picker: `#a,b,c ` (any of them).
/// Ticking several tags gathers pins; requiring all of them usually leaves none.
fn tag_query(tags: &[String]) -> String {
    format!("#{} ", tags.join(","))
}

use super::{App, Mode};
use crate::fs::EntryKind;
use crate::fs::path::Loc;
use crate::picker::{ItemKind, PickItem, Picker, PickerMode, Source};
use crate::pins::{Pin, PinKind};
use crate::platform::DriveKind;
use crate::prompt::Edit;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

/// Pending keys of picker normal mode (`gg`, `]t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerPending {
    G,
    Next,
    Prev,
    /// `d`: `dd` deletes, like in the file view.
    D,
}

/// Which picker to come back to after an edit started from one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reopen {
    Pins(String),
    Launcher(String),
    Tags,
    Machines(String),
}

/// Where a location lives, for `is:unc` / `is:local` / `on:`.
pub(super) struct Place {
    pub unc: bool,
    pub local: bool,
    pub server: Option<String>,
}

impl App {
    /// `<leader>p`: the pin picker, starting with `query` typed in.
    pub(super) fn open_pin_picker(&mut self, query: &str) {
        if !self.pins_loaded {
            self.set_status("pins are still loading".into(), false);
            return;
        }
        // Mapped drives tell `on:` which server a `P:\…` pin lives on.
        self.request(Loc::Root, false);
        let mapped = self.mapped_servers();
        let mut items: Vec<PickItem> = self
            .pins
            .pins
            .iter()
            .enumerate()
            .map(|(id, p)| self.pin_pick_item(id, p, &mapped))
            .collect();
        items.extend(self.pins.views.iter().enumerate().map(|(id, v)| PickItem {
            id,
            name: v.name.clone(),
            detail: v.query.clone(),
            kind: ItemKind::View,
            key: v.key_str().map(str::to_string),
            tags: Vec::new(),
            unc: false,
            local: false,
            reachable: None,
            server: None,
            score: 0,
        }));
        let picker = Picker::new("Pins", Source::Pins, items, self.worker.waker(), query);
        self.show_picker(picker);
    }

    /// `<leader><leader>` / `Ctrl-Space`: the launcher, over every pin (apps, web apps,
    /// folders, files, presets, machines, views), ranked by wtm's tiers then frecency.
    pub(super) fn open_launcher(&mut self, query: &str) {
        if !self.pins_loaded {
            self.set_status("pins are still loading".into(), false);
            return;
        }
        self.request(Loc::Root, false);
        let mapped = self.mapped_servers();
        let mut items: Vec<PickItem> = self
            .pins
            .pins
            .iter()
            .enumerate()
            .map(|(id, p)| self.pin_pick_item(id, p, &mapped))
            .collect();
        items.extend(self.pins.views.iter().enumerate().map(|(id, v)| PickItem {
            id,
            name: v.name.clone(),
            detail: v.query.clone(),
            kind: ItemKind::View,
            key: v.key_str().map(str::to_string),
            tags: Vec::new(),
            unc: false,
            local: false,
            reachable: None,
            server: None,
            score: 0,
        }));
        if self.launcher_start_menu {
            items.extend(self.start_menu_items());
        }
        items.extend(self.launcher_action_items());
        let picker = Picker::new(
            "Launcher",
            Source::Launcher,
            items,
            self.worker.waker(),
            query,
        );
        self.show_picker(picker);
    }

    /// `<leader>t` / `:tags`: every tag with its count.
    pub(super) fn open_tag_picker(&mut self) {
        if !self.pins_loaded {
            self.set_status("pins are still loading".into(), false);
            return;
        }
        let items: Vec<PickItem> = self
            .pins
            .tag_counts()
            .into_iter()
            .enumerate()
            .map(|(id, (tag, n))| PickItem {
                id,
                name: tag,
                detail: if n == 1 {
                    "1 pin".to_string()
                } else {
                    format!("{n} pins")
                },
                kind: ItemKind::Tag,
                key: None,
                tags: Vec::new(),
                unc: false,
                local: false,
                reachable: None,
                server: None,
                score: 0,
            })
            .collect();
        let picker = Picker::new("Tags", Source::Tags, items, self.worker.waker(), "");
        self.show_picker(picker);
    }

    pub(super) fn show_picker(&mut self, picker: Picker) {
        self.picker = Some(picker);
        self.picker_count = None;
        self.picker_pending = None;
        self.mode = Mode::Picker;
        self.schedule_preview();
    }

    pub(super) fn pin_pick_item(
        &self,
        id: usize,
        p: &Pin,
        mapped: &HashMap<char, String>,
    ) -> PickItem {
        let target = p.target().or_else(|| {
            let src = p.extra.get("source").and_then(|v| v.as_str())?;
            Loc::parse(src).ok()
        });
        let place = place_of(target.as_ref(), mapped);
        let reachable = target
            .as_ref()
            .and_then(|t| self.reach.get(t))
            .and_then(|r| {
                if r.is_reachable() {
                    Some(true)
                } else if r.is_unreachable() {
                    Some(false)
                } else {
                    None
                }
            });
        // A machine's dot is its port-445 probe.
        let reachable = match p.extra.get("host").and_then(|v| v.as_str()) {
            Some(host) if p.kind == PinKind::Machine => self.machine_status(host).0,
            _ => reachable,
        };
        // Its server is itself (`on:<host>`).
        let server = match p.extra.get("host").and_then(|v| v.as_str()) {
            Some(host) if p.kind == PinKind::Machine => Some(host.to_lowercase()),
            _ => place.server,
        };
        PickItem {
            id,
            name: p.name.clone(),
            detail: p.detail(),
            kind: ItemKind::Pin(p.kind),
            key: p.key_str().map(str::to_string),
            tags: p.tags.clone(),
            unc: place.unc,
            local: place.local,
            reachable,
            server,
            score: crate::frecency::pin_key(p)
                .map_or(0, |k| self.usage.score(&k, crate::frecency::now_secs())),
        }
    }

    /// Drive letter → server (lowercase), from the Locations listing's mapped drives.
    pub(super) fn mapped_servers(&self) -> HashMap<char, String> {
        let Some(listing) = self.dirs.get(&Loc::Root).and_then(|s| s.listing.as_ref()) else {
            return HashMap::new();
        };
        listing
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Drive(DriveKind::Network))
            .filter_map(|e| {
                let letter = e.name.chars().next()?.to_ascii_uppercase();
                let target = Loc::parse(&e.detail).ok()?;
                let server = target.as_fs()?.server()?.to_lowercase();
                Some((letter, server))
            })
            .collect()
    }

    pub(super) fn close_picker(&mut self) {
        if let Some(p) = self.picker.take()
            && p.source == Source::Pins
        {
            // `:view save <name>` saves this.
            self.last_picker_query = p.editor.text();
        }
        self.mode = Mode::Normal;
        self.cancel_unneeded();
    }

    /// The picker to come back to after an edit.
    fn reopen_here(&self) -> Option<Reopen> {
        let p = self.picker.as_ref()?;
        Some(match p.source {
            Source::Pins => Reopen::Pins(p.editor.text()),
            Source::Launcher => Reopen::Launcher(p.editor.text()),
            Source::Tags => Reopen::Tags,
            Source::Machines => Reopen::Machines(p.editor.text()),
            Source::Files
            | Source::Dirs
            | Source::Grep
            | Source::Commands
            | Source::Keymaps
            | Source::PinBackups => {
                return None;
            }
        })
    }

    pub(super) fn on_picker_key(&mut self, ev: KeyEvent) {
        let Some((mode, source)) = self.picker.as_ref().map(|p| (p.mode, p.source)) else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        // Both modes.
        match (ev.code, ctrl) {
            (KeyCode::Char('j' | 'n'), true) | (KeyCode::Down, _) => return self.picker_move(1),
            (KeyCode::Char('k' | 'p'), true) | (KeyCode::Up, _) => return self.picker_move(-1),
            (KeyCode::Enter, _) => {
                return self.picker_open(ev.modifiers.contains(KeyModifiers::SHIFT));
            }
            (KeyCode::Char('e'), true) => return self.picker_edit(false),
            (KeyCode::Char('t'), true) => return self.picker_edit(true),
            (KeyCode::Char('x'), true) => return self.picker_delete(),
            (KeyCode::Char('a'), true) => {
                if !self.picked_action('a') {
                    self.picker_pin();
                }
                return;
            }
            (KeyCode::Char('s'), true) => return self.toggle_start_menu(),
            // File / folder rows: open-with, reveal, copy the path.
            (KeyCode::Char(c @ ('o' | 'v' | 'y')), true) => {
                // A sync / backup preset: its two folders (the places menu).
                if !self.picked_action(c) && c == 'o' {
                    self.picker_places();
                }
                return;
            }
            // Files picker on a share: walk the whole tree.
            (KeyCode::Char('l'), true) if source == Source::Files => {
                self.close_picker();
                return self.open_files_picker(true);
            }
            // File and folder pickers: Tab chooses several (Enter yanks them).
            (KeyCode::Tab, _) if matches!(source, Source::Files | Source::Dirs | Source::Grep) => {
                return self.picker_with(|p| p.toggle_chosen());
            }
            // Tag picker: Tab chooses (multi-select).
            (KeyCode::Tab, _) if source == Source::Tags => {
                return self.picker_with(|p| p.toggle_chosen());
            }
            _ => {}
        }
        match mode {
            PickerMode::Insert => self.picker_insert_key(ev),
            PickerMode::Normal => self.picker_normal_key(ev),
        }
    }

    fn picker_insert_key(&mut self, ev: KeyEvent) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        match ev.code {
            KeyCode::Esc => {
                p.mode = PickerMode::Normal;
                return;
            }
            KeyCode::Tab => {
                p.accept_completion();
                return;
            }
            _ => {}
        }
        if let Edit::Changed = p.editor.handle(&ev, "") {
            p.text_changed();
            self.schedule_preview();
            self.grep_text_changed();
        }
    }

    fn picker_normal_key(&mut self, ev: KeyEvent) {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let pending = self.picker_pending.take();
        let count = self.picker_count.take();
        let n = count.unwrap_or(1).max(1) as isize;
        let half = self.picker.as_ref().map_or(5, |p| (p.page / 2).max(1)) as isize;
        // `O` on a sync / backup preset: its source and destination.
        if pending.is_none() && ev.code == KeyCode::Char('O') && !ctrl && self.picker_places() {
            return;
        }
        match (pending, ev.code, ctrl) {
            (Some(PickerPending::G), KeyCode::Char('g'), false) => {
                let row = count.map_or(0, |c| c.saturating_sub(1));
                self.picker_with(|p| p.set_cursor(row));
            }
            (Some(PickerPending::Next), KeyCode::Char('t'), false) => {
                self.picker_with(|p| p.cycle_tag(true));
            }
            (Some(PickerPending::Prev), KeyCode::Char('t'), false) => {
                self.picker_with(|p| p.cycle_tag(false));
            }
            (Some(PickerPending::D), KeyCode::Char('d'), false) => self.picker_delete(),
            (Some(_), _, _) => {}
            (None, KeyCode::Char(c), false)
                if c.is_ascii_digit() && (c != '0' || count.is_some()) =>
            {
                let d = c as usize - '0' as usize;
                self.picker_count = Some((count.unwrap_or(0) * 10 + d).min(99_999));
            }
            (None, KeyCode::Char('j'), false) => self.picker_move(n),
            (None, KeyCode::Char('k'), false) => self.picker_move(-n),
            (None, KeyCode::Char('d'), true) => self.picker_move(half * n),
            (None, KeyCode::Char('u'), true) => self.picker_move(-half * n),
            (None, KeyCode::Char('g'), false) => {
                self.picker_pending = Some(PickerPending::G);
                self.picker_count = count;
            }
            // `D` / `dd`: delete the pin (confirmed), the file view's delete keys.
            (None, KeyCode::Char('D'), false) => self.picker_delete(),
            (None, KeyCode::Char('d'), false) => self.picker_pending = Some(PickerPending::D),
            (None, KeyCode::Char('G'), false) => {
                let row = count.map_or(usize::MAX, |c| c.saturating_sub(1));
                self.picker_with(|p| p.set_cursor(row));
            }
            (None, KeyCode::Char(']'), false) => self.picker_pending = Some(PickerPending::Next),
            (None, KeyCode::Char('['), false) => self.picker_pending = Some(PickerPending::Prev),
            (None, KeyCode::Char('i' | 'a' | 'I' | 'A' | '/'), false) => {
                self.picker_with(|p| p.mode = PickerMode::Insert);
            }
            (None, KeyCode::Esc | KeyCode::Char('q'), false) => self.close_picker(),
            _ => {}
        }
    }

    fn picker_with(&mut self, f: impl FnOnce(&mut Picker)) {
        if let Some(p) = self.picker.as_mut() {
            f(p);
            self.schedule_preview();
        }
    }

    fn picker_move(&mut self, delta: isize) {
        self.picker_with(|p| p.move_cursor(delta));
    }

    fn picker_selected(&self) -> Option<(ItemKind, usize, String)> {
        let item = self.picker.as_ref()?.selected()?;
        Some((item.kind, item.id, item.name.clone()))
    }

    /// Enter; Shift-Enter (`skip_preview`) runs an update-mode sync preset at once.
    fn picker_open(&mut self, skip_preview: bool) {
        let Some(p) = self.picker.as_ref() else {
            return;
        };
        if p.source == Source::PinBackups {
            if let Some(idx) = p.selected().map(|i| i.id) {
                self.ask_restore_pins(idx);
            }
            return;
        }
        if p.source == Source::Tags {
            // The chosen tags (or the one under the cursor): pins with any of them.
            let mut tags: Vec<String> = p
                .view
                .iter()
                .filter(|i| p.is_chosen(i.id))
                .map(|i| i.name.clone())
                .collect();
            if tags.is_empty() {
                tags.extend(p.selected().map(|i| i.name.clone()));
            }
            if tags.is_empty() {
                return;
            }
            self.close_picker();
            self.open_pin_picker(&tag_query(&tags));
            return;
        }
        // The launcher with nothing matching: run what was typed (wtm), as Win+R would.
        if let Some(p) = self.picker.as_ref()
            && p.source == Source::Launcher
            && p.view.is_empty()
            && !p.running
        {
            let text = p.query.text.clone();
            if let Some(l) = crate::launcher::typed(&text) {
                self.close_picker();
                self.worker.launch(format!("ran {}", text.trim()), l);
            }
            return;
        }
        // The machine picker with no machine matching: what was typed is a new one.
        if let Some(p) = self.picker.as_ref()
            && p.source == Source::Machines
            && !p.query.text.trim().is_empty()
            && !p.view.iter().any(|i| i.kind != ItemKind::Action)
        {
            let text = p.query.text.clone();
            self.close_picker();
            if let Err(e) = self.open_machine_named(&text) {
                self.ask_machine_name(&text);
                if let Some(pr) = self.prompt.as_mut() {
                    pr.note = e;
                }
            }
            return;
        }
        let from_machines = self
            .picker
            .as_ref()
            .is_some_and(|p| p.source == Source::Machines);
        let Some((kind, id, _)) = self.picker_selected() else {
            return;
        };
        let chosen: Vec<usize> = self
            .picker
            .as_ref()
            .filter(|p| matches!(p.source, Source::Files | Source::Dirs | Source::Grep))
            .map(|p| p.chosen.clone())
            .unwrap_or_default();
        let typed = self
            .picker
            .as_ref()
            .map(|p| p.query.text.clone())
            .unwrap_or_default();
        let host = self
            .picker
            .as_ref()
            .and_then(|p| p.selected())
            .map(|i| i.name.clone());
        self.close_picker();
        match kind {
            ItemKind::StartMenu => self.launch_start_menu(id),
            ItemKind::Action if from_machines => self.ask_machine_name(&typed),
            ItemKind::Action => self.run_launcher_action(id, &typed),
            ItemKind::File | ItemKind::Dir => self.open_picked(id, chosen),
            ItemKind::Binding | ItemKind::Command => self.run_picked_command(kind, id),
            ItemKind::View => {
                if let Some(q) = self.pins.views.get(id).map(|v| v.query.clone()) {
                    self.open_pin_picker(&format!("{q} "));
                }
            }
            ItemKind::Machine => match host.as_deref().map(Loc::machine) {
                Some(Ok(loc)) => self.navigate(loc, None),
                Some(Err(e)) => self.set_status(e.to_string(), true),
                None => {}
            },
            ItemKind::Pin(PinKind::Sync) => self.open_sync_pin(id, skip_preview),
            _ => self.open_pin(id),
        }
    }

    /// `^a`: pin the machine under the cursor (machine picker).
    fn picker_pin(&mut self) {
        // The launcher: pin the Start Menu app under the cursor, else a web app (the
        // typed text as its URL when it looks like one).
        if let Some(p) = self
            .picker
            .as_ref()
            .filter(|p| p.source == Source::Launcher)
        {
            let text = p.query.text.clone();
            let start = p
                .selected()
                .filter(|i| i.kind == ItemKind::StartMenu)
                .map(|i| (i.name.clone(), i.detail.clone()));
            self.close_picker();
            return match start {
                Some((name, lnk)) => self.new_app_pin(&name, &lnk),
                None => self.new_web_pin(&text),
            };
        }
        let Some((kind, _, name)) = self.picker_selected() else {
            return;
        };
        match kind {
            ItemKind::Machine => {
                self.close_picker();
                self.pin_machine(&name, None);
            }
            ItemKind::Pin(_) => self.set_status(format!("{name} is pinned already"), false),
            _ => self.set_status("^a pins a machine".into(), false),
        }
    }

    /// `^e` (all of it) or `^t` (tags only); the picker comes back after.
    fn picker_edit(&mut self, tags_only: bool) {
        let Some((kind, id, name)) = self.picker_selected() else {
            return;
        };
        let reopen = self.reopen_here();
        match kind {
            ItemKind::Pin(_) => {
                self.close_picker();
                self.edit_pin(id, tags_only, reopen);
            }
            ItemKind::View if tags_only => {
                self.set_status("views have no tags; ^e renames it".into(), false);
            }
            ItemKind::View => {
                self.close_picker();
                self.edit_view(id, reopen);
            }
            ItemKind::Tag => {
                self.close_picker();
                self.start_tag_rename(&name);
            }
            ItemKind::Backup => {
                self.set_status("a backup can't be edited: Enter restores it".into(), false);
            }
            ItemKind::Machine
            | ItemKind::StartMenu
            | ItemKind::Action
            | ItemKind::File
            | ItemKind::Dir
            | ItemKind::Binding
            | ItemKind::Command => {
                self.set_status(format!("{name} isn't pinned: ^a pins it"), false);
            }
        }
    }

    fn picker_delete(&mut self) {
        let Some((kind, id, name)) = self.picker_selected() else {
            return;
        };
        if kind == ItemKind::Backup {
            // The newest five rotate out by themselves; none is deleted by hand here.
            return self.set_status("backups rotate by themselves (newest 5 kept)".into(), false);
        }
        if matches!(
            kind,
            ItemKind::Machine
                | ItemKind::StartMenu
                | ItemKind::Action
                | ItemKind::File
                | ItemKind::Dir
                | ItemKind::Binding
                | ItemKind::Command
        ) {
            // Recent / inventory rows aren't sentier's to delete.
            return self.set_status(format!("{name} isn't pinned: nothing to delete"), false);
        }
        let reopen = self.reopen_here();
        self.close_picker();
        match kind {
            ItemKind::Pin(_) => self.confirm_unpin(id, reopen),
            ItemKind::View => self.confirm_view_delete(id, reopen),
            ItemKind::Tag => self.tag_delete(&name, reopen),
            _ => {}
        }
    }

    /// The folder the picker's preview lists, if the selection is a folder pin not
    /// known to be unreachable.
    pub(super) fn picker_preview_loc(&self) -> Option<Loc> {
        let item = self.picker.as_ref()?.selected()?;
        if item.kind != ItemKind::Pin(PinKind::Folder) || item.reachable == Some(false) {
            return None;
        }
        self.pins.pins.get(item.id)?.target()
    }
}

/// Where `target` lives: a share (with its server), a mapped drive (server from the
/// mapping), or a local disk.
pub(super) fn place_of(target: Option<&Loc>, mapped: &HashMap<char, String>) -> Place {
    let fs = target.and_then(Loc::as_fs);
    let letter = fs.and_then(|f| f.drive());
    let mapped_server = letter.and_then(|l| mapped.get(&l).cloned());
    let server = match target {
        Some(Loc::Server(s)) => Some(s.to_lowercase()),
        _ => fs
            .and_then(|f| f.server().map(str::to_lowercase))
            .or(mapped_server.clone()),
    };
    Place {
        unc: fs.is_some_and(|f| f.is_network()) || matches!(target, Some(Loc::Server(_))),
        local: letter.is_some() && mapped_server.is_none(),
        server,
    }
}

impl App {
    /// The Locations entries the `f` filter keeps. It takes the pin query: tags and
    /// `is:` / `on:` filters, then every remaining word as a case-insensitive substring
    /// of the name or its detail (label, mapped target, pin name).
    pub(super) fn root_visible(&self, listing: &crate::fs::Listing) -> Vec<usize> {
        let q = crate::picker::query::PinQuery::parse(&self.filter);
        let words: Vec<String> = q.text.split_whitespace().map(str::to_lowercase).collect();
        let mapped = self.mapped_servers();
        let no_tags: Vec<String> = Vec::new();
        listing
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let target = App::root_target(e);
                let place = place_of(target.as_ref(), &mapped);
                let pin = self.pin_of(e);
                let facts = crate::picker::query::Facts {
                    kind: pin.map(|p| p.kind),
                    view: false,
                    machine: matches!(e.kind, EntryKind::Machine(_)),
                    online: match e.kind {
                        EntryKind::Machine(online) => online,
                        _ => None,
                    },
                    tags: pin.map_or(&no_tags, |p| &p.tags),
                    unc: place.unc,
                    local: place.local,
                    unreachable: target
                        .as_ref()
                        .and_then(|t| self.reach.get(t))
                        .is_some_and(|r| r.is_unreachable()),
                    server: place.server,
                };
                let hay = format!("{} {}", e.name, e.detail).to_lowercase();
                q.matches(&facts) && words.iter().all(|w| hay.contains(w.as_str()))
            })
            .map(|(i, _)| i)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::tag_query;
    use crate::picker::query::{Facts, PinQuery};
    use crate::pins::PinKind;

    /// Ticking `#code #library #mainpool` used to require all three (no pin had them):
    /// now it's pins with any of them.
    #[test]
    fn chosen_tags_find_pins_with_any_of_them() {
        let chosen = ["code", "library", "mainpool"].map(String::from);
        let text = tag_query(&chosen);
        assert_eq!(text, "#code,library,mainpool ");
        let q = PinQuery::parse(&text);
        let has = |tags: &[&str]| {
            let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
            q.matches(&Facts {
                kind: Some(PinKind::Folder),
                view: false,
                machine: false,
                online: None,
                tags: &tags,
                unc: false,
                local: true,
                unreachable: false,
                server: None,
            })
        };
        assert!(has(&["code"]) && has(&["library", "web"]) && has(&["mainpool"]));
        assert!(!has(&["web", "game"]));
        assert_eq!(tag_query(&["web".into()]), "#web ");
    }
}
