//! The pin picker (`<leader>p`, CLAUDE.md sections 3 and 7) in the app: building its
//! items from the pin store, and its keys.
//!
//! Insert mode (it opens there) types into the query; `Esc` goes to normal mode, where
//! `j`/`k` (with counts), `gg`/`G`, `^d`/`^u`, `]t`/`[t` work, `i`/`a` go back to insert,
//! and `Esc`/`q` close. In both: `^j`/`^k` `^n`/`^p` move, `Enter` opens, `^e` edits,
//! `^t` edits tags, `^x` unpins (asks first). Editing returns to the picker.

use super::{App, Mode};
use crate::fs::EntryKind;
use crate::fs::path::Loc;
use crate::picker::{PickItem, Picker, PickerMode};
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
        let items: Vec<PickItem> = self
            .pins
            .pins
            .iter()
            .enumerate()
            .map(|(id, p)| {
                let target = p.target().or_else(|| {
                    let src = p.extra.get("source").and_then(|v| v.as_str())?;
                    Loc::parse(src).ok()
                });
                let fs = target.as_ref().and_then(Loc::as_fs);
                let letter = fs.and_then(|f| f.drive());
                let mapped_server = letter.and_then(|l| mapped.get(&l).cloned());
                let server = fs
                    .and_then(|f| f.server().map(str::to_lowercase))
                    .or(mapped_server.clone());
                PickItem {
                    id,
                    name: p.name.clone(),
                    detail: p.detail(),
                    kind: p.kind,
                    key: p.key_char(),
                    tags: p.tags.clone(),
                    unc: fs.is_some_and(|f| f.is_network()),
                    local: letter.is_some() && mapped_server.is_none(),
                    unreachable: target
                        .as_ref()
                        .and_then(|t| self.reach.get(t))
                        .is_some_and(|r| r.is_unreachable()),
                    server,
                }
            })
            .collect();
        let picker = Picker::new("Pins", items, self.worker.waker(), query);
        self.picker = Some(picker);
        self.picker_count = None;
        self.picker_pending = None;
        self.mode = Mode::Picker;
        self.schedule_preview();
    }

    /// Drive letter → server (lowercase), from the Locations listing's mapped drives.
    fn mapped_servers(&self) -> HashMap<char, String> {
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
        self.picker = None;
        self.mode = Mode::Normal;
        self.cancel_unneeded();
    }

    /// The picker's query, to come back to it after an edit.
    fn picker_query(&self) -> String {
        self.picker
            .as_ref()
            .map(|p| p.editor.text())
            .unwrap_or_default()
    }

    pub(super) fn on_picker_key(&mut self, ev: KeyEvent) {
        let Some(mode) = self.picker.as_ref().map(|p| p.mode) else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        // Both modes.
        match (ev.code, ctrl) {
            (KeyCode::Char('j' | 'n'), true) | (KeyCode::Down, _) => return self.picker_move(1),
            (KeyCode::Char('k' | 'p'), true) | (KeyCode::Up, _) => return self.picker_move(-1),
            (KeyCode::Enter, _) => return self.picker_open(),
            (KeyCode::Char('e'), true) => return self.picker_edit(false),
            (KeyCode::Char('t'), true) => return self.picker_edit(true),
            (KeyCode::Char('x'), true) => return self.picker_delete(),
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
        }
    }

    fn picker_normal_key(&mut self, ev: KeyEvent) {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let pending = self.picker_pending.take();
        let count = self.picker_count.take();
        let n = count.unwrap_or(1).max(1) as isize;
        let half = self.picker.as_ref().map_or(5, |p| (p.page / 2).max(1)) as isize;
        let code = ev.code;
        match (pending, code, ctrl) {
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

    fn picker_selected(&self) -> Option<usize> {
        self.picker.as_ref()?.selected().map(|i| i.id)
    }

    fn picker_open(&mut self) {
        let Some(id) = self.picker_selected() else {
            return;
        };
        self.close_picker();
        self.open_pin(id);
    }

    /// `^e` (name, tags, key) or `^t` (tags only); the picker comes back after.
    fn picker_edit(&mut self, tags_only: bool) {
        let Some(id) = self.picker_selected() else {
            return;
        };
        let query = self.picker_query();
        self.close_picker();
        self.edit_pin(id, tags_only, Some(query));
    }

    fn picker_delete(&mut self) {
        let Some(id) = self.picker_selected() else {
            return;
        };
        let query = self.picker_query();
        self.close_picker();
        self.confirm_unpin(id, Some(query));
    }

    /// The folder the picker's preview lists, if the selection is a reachable folder pin.
    pub(super) fn picker_preview_loc(&self) -> Option<Loc> {
        let item = self.picker.as_ref()?.selected()?;
        if item.kind != crate::pins::PinKind::Folder || item.unreachable {
            return None;
        }
        self.pins.pins.get(item.id)?.target()
    }
}
