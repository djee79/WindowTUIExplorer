//! Pins in the explorer (CLAUDE.md section 3): `m{key}`, `'{key}`, `M`, and pin
//! editing in Locations (`r` rename, `T` tags, `dd` unpin).
//!
//! New pins go through prompts: name → tags → key (the key step is skipped for `m{key}`,
//! which already has one). The draft lives in [`PinDraft`] until the last step, so `Esc`
//! at any step pins nothing. Every change saves the whole store through the worker.

use super::keys::PromptKind;
use super::picker::Reopen;
use super::{App, Loc};
use crate::fs::{Entry, EntryKind};
use crate::pins::{self, KeyOwner, Pin, PinKind, PinStore};

#[derive(Debug, Clone)]
pub struct PinDraft {
    pin: Pin,
    /// Editing this pin (rename / tags / unpin), None for a new one.
    editing: Option<usize>,
    /// The key: from `m{key}`, the pin being edited, or the key step.
    key: Option<char>,
    /// End with the key step (`M`, `^e` in the picker); `m{key}` already has one.
    ask_key: bool,
    /// One step only (rename or tags), then save.
    single: bool,
    /// Started from a picker: reopen it when done.
    reopen: Option<Reopen>,
}

impl PinDraft {
    fn new(pin: Pin, key: Option<char>, ask_key: bool) -> PinDraft {
        PinDraft {
            pin,
            editing: None,
            key,
            ask_key,
            single: false,
            reopen: None,
        }
    }
}

impl App {
    /// Pinned-folder entries for the Locations root, in pin order.
    pub(super) fn pin_entries(&self) -> Vec<Entry> {
        self.pins
            .folders()
            .map(|(p, loc)| Entry::virtual_entry(loc.display(), EntryKind::Pin, p.name.clone()))
            .collect()
    }

    /// The pin behind a Locations entry.
    pub fn pin_of(&self, e: &Entry) -> Option<&Pin> {
        let idx = self.pin_index(e)?;
        self.pins.pins.get(idx)
    }

    fn pin_index(&self, e: &Entry) -> Option<usize> {
        if e.kind != EntryKind::Pin {
            return None;
        }
        let target = App::root_target(e)?;
        self.pins.find(PinKind::Folder, &target)
    }

    pub(super) fn on_pins_loaded(&mut self, result: Result<(PinStore, Vec<String>), String>) {
        match result {
            Ok((store, problems)) => {
                for p in &problems {
                    tracing::warn!(problem = %p, "pins.toml");
                }
                if let Some(first) = problems.first() {
                    let more = match problems.len() {
                        1 => String::new(),
                        n => format!(" (+{} more in the log)", n - 1),
                    };
                    self.set_status(format!("{first}{more}"), true);
                }
                self.pins = store;
                self.pins_loaded = true;
                self.reload_root();
            }
            // Keep running without pins, and never save over a file we couldn't read.
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read pins");
                self.set_status(format!("couldn't read pins: {e}"), true);
            }
        }
    }

    /// Pins can change only once the store has been read; otherwise a save would
    /// overwrite pins we never saw.
    pub(super) fn pins_ready(&mut self) -> bool {
        if !self.pins_loaded {
            self.set_status(
                "pins.toml isn't loaded (see the message above); not changing it".into(),
                true,
            );
        }
        self.pins_loaded
    }

    pub(super) fn save_pins(&mut self) {
        self.worker.save_pins(self.pins.clone());
        self.reload_root();
    }

    // ------------------------------------------------------------------ actions

    /// `m{c}`: pin the current folder under `c`. An existing pin of this folder just
    /// gets the key; a new one asks for a name and tags first.
    pub(super) fn set_mark(&mut self, c: char) {
        if !pins::valid_key(c) {
            self.set_status(format!("m{c}: pin keys are letters or digits"), true);
            return;
        }
        let loc = match &self.cwd {
            Loc::Fs(_) | Loc::Server(_) => self.cwd.clone(),
            Loc::Root => {
                self.set_status("Locations can't be pinned (gl gets you here)".into(), true);
                return;
            }
            Loc::Results { .. } => {
                self.set_status(
                    "search results can't be pinned; M pins the result under the cursor".into(),
                    true,
                );
                return;
            }
            Loc::Machine(host) => {
                let host = host.clone();
                return self.pin_machine(&host, Some(c));
            }
            Loc::Machines => {
                self.set_status("M on a machine pins it".into(), false);
                return;
            }
        };
        if !self.pins_ready() {
            return;
        }
        if let Some(idx) = self.pins.find(PinKind::Folder, &loc) {
            let moved = self.pins.set_key(idx, Some(c));
            let name = self.pins.pins[idx].name.clone();
            self.save_pins();
            self.set_status(pinned_message(c, &name, moved.as_deref()), false);
            return;
        }
        let pin = Pin::for_loc(PinKind::Folder, &loc.name(), &loc);
        self.start_draft(PinDraft::new(pin, Some(c), false));
    }

    /// `:pin`: pin the current folder, asking for name, tags and key.
    pub(super) fn pin_cwd(&mut self) {
        let loc = match &self.cwd {
            Loc::Fs(_) | Loc::Server(_) => self.cwd.clone(),
            _ => {
                self.set_status("only a folder can be pinned (M pins an item)".into(), true);
                return;
            }
        };
        if !self.pins_ready() {
            return;
        }
        if let Some(idx) = self.pins.find(PinKind::Folder, &loc) {
            self.edit_pin(idx, false, None);
            return;
        }
        let pin = Pin::for_loc(PinKind::Folder, &loc.name(), &loc);
        self.start_draft(PinDraft::new(pin, None, true));
    }

    /// `'{c}`: open the pin with that key, or the pin picker with that view's query.
    pub(super) fn jump_mark(&mut self, c: char) {
        match self.pins.key_owner(c) {
            Some(KeyOwner::Pin(idx)) => self.open_pin(idx),
            Some(KeyOwner::View(idx)) => {
                if let Some(q) = self.pins.views.get(idx).map(|v| v.query.clone()) {
                    self.open_pin_picker(&format!("{q} "));
                }
            }
            None if self.pins_loaded => {
                self.set_status(format!("no pin on '{c} (m{c} pins this folder)"), false)
            }
            None => self.set_status("pins are still loading".into(), false),
        }
    }

    /// Go to a folder pin, open a file pin. Other kinds run from the launcher (M5) or
    /// as jobs (M4b).
    pub(super) fn open_pin(&mut self, idx: usize) {
        let Some(pin) = self.pins.pins.get(idx).cloned() else {
            return;
        };
        match pin.kind {
            PinKind::Machine => return self.open_machine_pin(&pin),
            PinKind::Sync => return self.open_sync_pin(idx, false),
            _ => {}
        }
        let Some(target) = pin.target() else {
            let when = match pin.kind {
                PinKind::Backup => "the next M4b slice",
                _ => "M5",
            };
            self.set_status(
                format!(
                    "{} is a {} pin; those run in {when}",
                    pin.name,
                    pin.kind.label()
                ),
                false,
            );
            return;
        };
        match pin.kind {
            PinKind::Folder => {
                // Known unreachable: say so and stay put, and check again for next time.
                if let Some(Err(e)) = self.reach.get(&target).and_then(|r| r.last.clone()) {
                    self.set_status(format!("{} is {e}; probing again…", pin.name), true);
                    self.probe(target);
                    return;
                }
                self.navigate(target, None);
            }
            PinKind::File => {
                self.set_status(format!("opening {}…", pin.name), false);
                self.worker.open(target);
            }
            _ => self.set_status(
                format!(
                    "{} is an app pin; apps run from the launcher (M5)",
                    pin.name
                ),
                false,
            ),
        }
    }

    /// `M`: pin the item under the cursor (folder → folder pin, .exe/.bat/.lnk → app,
    /// other files → file pin).
    pub(super) fn pin_item(&mut self) {
        // A machine under the cursor (Machines, Locations) or the machine being viewed.
        if let Some(host) = self.machine_here() {
            return self.pin_machine(&host, None);
        }
        let Some(entry) = self.selected().cloned() else {
            self.set_status("nothing under the cursor to pin".into(), false);
            return;
        };
        if entry.kind == EntryKind::Pin {
            self.set_status(
                "already pinned (r renames, T edits tags, dd unpins)".into(),
                false,
            );
            return;
        }
        let target = match self.cwd.join(&entry.name) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(e.to_string(), true);
                return;
            }
        };
        if !self.pins_ready() {
            return;
        }
        let kind = PinKind::for_item(&entry.name, entry.is_container());
        if let Some(idx) = self.pins.find(kind, &target) {
            let name = self.pins.pins[idx].name.clone();
            self.set_status(format!("already pinned as {name}"), false);
            return;
        }
        let pin = Pin::for_loc(kind, &target.name(), &target);
        self.start_draft(PinDraft::new(pin, None, true));
    }

    /// The pin under the cursor in Locations, for `r` / `T` / `dd`.
    fn selected_pin(&mut self, what: &str) -> Option<usize> {
        let idx = self.selected().and_then(|e| self.pin_index(e));
        if idx.is_none() {
            self.set_status(
                format!("{what} works on a pinned folder in Locations"),
                false,
            );
        }
        idx.filter(|_| self.pins_ready())
    }

    pub(super) fn rename_pin(&mut self) {
        if !self.at_root() {
            self.set_status(
                "renaming files arrives with file operations (M6)".into(),
                false,
            );
            return;
        }
        let Some(idx) = self.selected_pin("r") else {
            return;
        };
        let Some(mut d) = self.edit_draft(idx, None) else {
            return;
        };
        d.single = true;
        let name = d.pin.name.clone();
        self.pin_draft = Some(d);
        self.open_prompt_with(PromptKind::PinName, &name);
    }

    pub(super) fn edit_pin_tags(&mut self) {
        if let Some(idx) = self.selected_pin("T") {
            self.edit_pin(idx, true, None);
        }
    }

    /// `dd` on a pin in Locations: asks first (a pin carries a name, tags and a key).
    pub(super) fn unpin_selected(&mut self) {
        if let Some(idx) = self.selected_pin("dd") {
            self.confirm_unpin(idx, None);
        }
    }

    fn edit_draft(&self, idx: usize, reopen: Option<Reopen>) -> Option<PinDraft> {
        let pin = self.pins.pins.get(idx)?.clone();
        Some(PinDraft {
            key: pin.key_char(),
            pin,
            editing: Some(idx),
            ask_key: true,
            single: false,
            reopen,
        })
    }

    /// Edit pin `idx`: all of it (name → tags → key), or only its tags. `reopen` is the
    /// picker query to come back to.
    pub(super) fn edit_pin(&mut self, idx: usize, tags_only: bool, reopen: Option<Reopen>) {
        if !self.pins_ready() {
            return;
        }
        let Some(mut d) = self.edit_draft(idx, reopen) else {
            return;
        };
        d.single = tags_only;
        let (kind, text) = if tags_only {
            (PromptKind::PinTags, d.pin.tags.join(" "))
        } else {
            (PromptKind::PinName, d.pin.name.clone())
        };
        self.pin_draft = Some(d);
        self.open_prompt_with(kind, &text);
    }

    /// Ask, then unpin `idx`.
    pub(super) fn confirm_unpin(&mut self, idx: usize, reopen: Option<Reopen>) {
        if !self.pins_ready() {
            return;
        }
        let Some(mut d) = self.edit_draft(idx, reopen) else {
            return;
        };
        d.single = true;
        self.pin_draft = Some(d);
        self.open_prompt_with(PromptKind::ConfirmUnpin, "");
    }

    // ------------------------------------------------------------------ prompts

    /// A new machine pin through the usual prompts (name, tags, then a key unless given).
    pub(super) fn pin_machine_draft(&mut self, pin: Pin, key: Option<char>) {
        self.start_draft(PinDraft::new(pin, key, key.is_none()));
    }

    fn start_draft(&mut self, draft: PinDraft) {
        let name = draft.pin.name.clone();
        self.pin_draft = Some(draft);
        self.open_prompt_with(PromptKind::PinName, &name);
    }

    /// The question a pin prompt asks, shown after the prompt prefix.
    pub fn pin_prompt_note(&self, kind: PromptKind) -> String {
        if kind == PromptKind::Confirm {
            return self.confirm_note();
        }
        let Some(d) = &self.pin_draft else {
            return String::new();
        };
        match kind {
            PromptKind::ConfirmUnpin => {
                format!(
                    "unpin {} ({})? y = yes, anything else keeps it",
                    d.pin.name,
                    d.pin.detail()
                )
            }
            _ => String::new(),
        }
    }

    /// Enter on a pin prompt. Err keeps the prompt open with that note.
    pub(super) fn pin_submit(&mut self, kind: PromptKind, text: &str) -> Result<(), String> {
        if matches!(
            kind,
            PromptKind::Confirm
                | PromptKind::ViewName
                | PromptKind::ViewKey
                | PromptKind::TagRename
        ) {
            return self.store_submit(kind, text);
        }
        let Some(mut d) = self.pin_draft.take() else {
            return Ok(());
        };
        let next = match kind {
            PromptKind::PinName => {
                let name = text.trim();
                if name.is_empty() {
                    self.pin_draft = Some(d);
                    return Err("a pin needs a name".to_string());
                }
                d.pin.name = name.to_string();
                Some(PromptKind::PinTags)
            }
            PromptKind::PinTags => match pins::parse_tags(text) {
                Ok(tags) => {
                    d.pin.tags = tags;
                    d.ask_key.then_some(PromptKind::PinKey)
                }
                Err(e) => {
                    self.pin_draft = Some(d);
                    return Err(e);
                }
            },
            PromptKind::PinKey => {
                let t = text.trim();
                let mut chars = t.chars();
                match (chars.next(), chars.next()) {
                    (None, _) => d.key = None,
                    (Some(c), None) if pins::valid_key(c) => d.key = Some(c),
                    _ => {
                        self.pin_draft = Some(d);
                        return Err("one letter or digit, or empty for none".to_string());
                    }
                }
                None
            }
            PromptKind::ConfirmUnpin => {
                if let Some(idx) = d.editing {
                    self.unpin(idx);
                }
                self.reopen_picker(d.reopen);
                return Ok(());
            }
            _ => None,
        };
        match next {
            Some(next) if !d.single => {
                let prefill = match next {
                    PromptKind::PinTags => d.pin.tags.join(" "),
                    PromptKind::PinKey => d.key.map(String::from).unwrap_or_default(),
                    _ => String::new(),
                };
                self.pin_draft = Some(d);
                self.open_prompt_with(next, &prefill);
            }
            _ => self.finish_draft(d),
        }
        Ok(())
    }

    pub(super) fn pin_cancel(&mut self, kind: PromptKind) {
        if matches!(
            kind,
            PromptKind::Confirm
                | PromptKind::ViewName
                | PromptKind::ViewKey
                | PromptKind::TagRename
        ) {
            return self.store_cancel(kind);
        }
        if let Some(d) = self.pin_draft.take() {
            let msg = match (kind, d.editing) {
                (PromptKind::ConfirmUnpin, _) => format!("kept {}", d.pin.name),
                (_, Some(_)) => "unchanged".to_string(),
                (_, None) => "not pinned".to_string(),
            };
            self.set_status(msg, false);
            self.reopen_picker(d.reopen);
        }
    }

    pub(super) fn reopen_picker(&mut self, reopen: Option<Reopen>) {
        match reopen {
            Some(Reopen::Pins(q)) => self.open_pin_picker(&q),
            Some(Reopen::Tags) => self.open_tag_picker(),
            Some(Reopen::Machines(q)) => self.open_machine_picker(&q),
            None => {}
        }
    }

    fn finish_draft(&mut self, d: PinDraft) {
        let name = d.pin.name.clone();
        let (idx, edited) = match d.editing {
            Some(idx) if idx < self.pins.pins.len() => {
                self.pins.pins[idx] = d.pin;
                (idx, true)
            }
            Some(_) => return,
            None => {
                self.pins.pins.push(d.pin);
                (self.pins.pins.len() - 1, false)
            }
        };
        let moved = if d.ask_key || !edited {
            self.pins.set_key(idx, d.key)
        } else {
            None
        };
        self.save_pins();
        let msg = match (d.key, edited) {
            (Some(c), _) if d.ask_key || !edited => pinned_message(c, &name, moved.as_deref()),
            (_, true) => format!("saved {name}"),
            (None, false) => format!("pinned {name}"),
            (Some(_), false) => format!("pinned {name}"),
        };
        self.set_status(msg, false);
        self.reopen_picker(d.reopen);
    }

    fn unpin(&mut self, idx: usize) {
        if idx >= self.pins.pins.len() {
            return;
        }
        let pin = self.pins.pins.remove(idx);
        if let Some(t) = pin.target() {
            self.reach.remove(&t);
        }
        self.save_pins();
        self.set_status(format!("unpinned {}", pin.name), false);
    }

    /// The tags matching the word being typed, most used first: `#aveva 23 · #acme 11`.
    pub(super) fn tag_candidates(&self, text: &str) -> Vec<(String, usize)> {
        let word = last_word(text).trim_start_matches('#').to_lowercase();
        let typed: Vec<&str> = text.split([' ', ',']).collect();
        self.pins
            .tag_counts()
            .into_iter()
            .filter(|(t, _)| t.starts_with(&word) && !typed.contains(&t.as_str()))
            .collect()
    }

    /// Tab in the tags prompt: complete the word being typed with the best candidate.
    pub(super) fn complete_tag(&self, text: &str) -> Option<String> {
        let (best, _) = self.tag_candidates(text).into_iter().next()?;
        let head = &text[..text.len() - last_word(text).len()];
        Some(format!("{head}{best} "))
    }
}

fn last_word(text: &str) -> &str {
    text.rsplit([' ', ',']).next().unwrap_or_default()
}

fn pinned_message(c: char, name: &str, moved: Option<&str>) -> String {
    match moved {
        Some(old) if old != name => format!("'{c} → {name} (was {old})"),
        _ => format!("'{c} → {name}"),
    }
}
