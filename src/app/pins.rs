//! Pins in the explorer (CLAUDE.md section 3): `m{key}`, `'{key}`, `M`, and pin
//! editing in Locations (`r` rename, `T` tags, `dd` unpin).
//!
//! New pins go through prompts: name → tags → key (the key step is skipped for `m{key}`,
//! which already has one). The draft lives in [`PinDraft`] until the last step, so `Esc`
//! at any step pins nothing. Every change saves the whole store through the worker.

use super::keys::{PresetField, PromptKind};
use super::picker::Reopen;
use super::{App, Loc};
use crate::fs::{Entry, EntryKind};
use crate::pins::{self, KeyOwner, Pin, PinKind, PinStore};

#[derive(Debug, Clone)]
pub struct PinDraft {
    pub(super) pin: Pin,
    /// Editing this pin (rename / tags / unpin), None for a new one.
    editing: Option<usize>,
    /// The key: from `m{key}`, the pin being edited, or the key step.
    key: Option<String>,
    /// End with the key step (`M`, `^e` in the picker); `m{key}` already has one.
    ask_key: bool,
    /// One step only (rename or tags), then save.
    single: bool,
    /// Started from a picker: reopen it when done.
    reopen: Option<Reopen>,
    /// A sync / backup preset's fields still to ask, between the name and the tags.
    fields: Vec<PresetField>,
    /// A new preset: its folders are picked first, the name asked after them (with a
    /// suggestion from both folders).
    name_last: bool,
}

/// Giving a key that clashes with others (`'dd` while `'d` is D:\, or `'d` while `'dd`
/// and `'dl` exist): each of those is asked for a new key in turn, and only then is
/// anything saved. `Esc` at any of them saves nothing.
#[derive(Debug, Clone)]
pub struct KeyMove {
    /// The key being given, and to whom (by name, for the notes).
    key: String,
    name: String,
    /// The pin or view getting it, when it's already in the store.
    except: Option<KeyOwner>,
    /// The ones still to move; the first is being asked.
    queue: Vec<KeyOwner>,
    /// New keys chosen so far (None: no key any more).
    moves: Vec<(KeyOwner, Option<String>)>,
    then: KeyThen,
}

/// What a [`KeyMove`] finishes once everyone has moved.
#[derive(Debug, Clone)]
enum KeyThen {
    /// Save this pin draft (new or edited) with its key.
    Draft(Box<PinDraft>),
    /// `m{key}` on a folder or machine that's already pinned: give pin `idx` the key.
    Mark(usize),
    /// A saved view's key step.
    View(super::tags::ViewDraft),
}

impl PinDraft {
    fn new(pin: Pin, key: Option<String>, ask_key: bool) -> PinDraft {
        PinDraft {
            pin,
            editing: None,
            key,
            ask_key,
            single: false,
            reopen: None,
            fields: Vec::new(),
            name_last: false,
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
        let k = c.to_string();
        if !pins::valid_key(&k) {
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
                return self.pin_machine(&host, Some(k));
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
            return self.give_mark(idx, k);
        }
        let pin = Pin::for_loc(PinKind::Folder, &loc.name(), &loc);
        self.start_draft(PinDraft::new(pin, Some(k), false));
    }

    /// A saved view's key step: like a pin's, clashing keys move first.
    pub(super) fn give_view_key(&mut self, d: super::tags::ViewDraft, k: Option<String>) {
        match k {
            Some(k) => {
                let (name, except) = (d.name().to_string(), Some(KeyOwner::View(d.idx())));
                self.give_key(k, name, except, KeyThen::View(d));
            }
            None => self.finish_view_key(d, None),
        }
    }

    /// `m{key}` on something already pinned: it gets the key (moving clashing ones
    /// first).
    pub(super) fn give_mark(&mut self, idx: usize, k: String) {
        let Some(name) = self.pins.pins.get(idx).map(|p| p.name.clone()) else {
            return;
        };
        self.give_key(k, name, Some(KeyOwner::Pin(idx)), KeyThen::Mark(idx));
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
    /// When two-letter keys start with `c` (`'dd`, `'dl`), wait for the second letter;
    /// a key never both is one and starts others, so this never has to guess.
    pub(super) fn jump_mark(&mut self, c: char) {
        if self.pins.key_owner(&c.to_string()).is_none() && self.pins.starts_keys(c) {
            self.mark_prefix = Some(c);
            self.keys_since = Some(std::time::Instant::now());
            return;
        }
        self.jump_key(&c.to_string());
    }

    /// The second letter after `'d`: Esc cancels, Backspace goes back to `'`.
    pub(super) fn mark_second_key(&mut self, first: char, key: &crate::keymap::Key) {
        use ratatui::crossterm::event::KeyCode;
        let shown = self.which_key;
        self.clear_keys();
        match (key.code, key.plain_char()) {
            (KeyCode::Esc, _) => {}
            (KeyCode::Backspace, _) => {
                self.keys_pending
                    .push(crate::keymap::Key::plain(KeyCode::Char('\'')));
                self.keys_since = Some(std::time::Instant::now());
                self.which_key = shown;
            }
            (_, Some(second)) => self.jump_key(&format!("{first}{second}")),
            _ => self.set_status(format!("'{first}: no pin there"), false),
        }
    }

    fn jump_key(&mut self, k: &str) {
        match self.pins.key_owner(k) {
            Some(KeyOwner::Pin(idx)) => self.open_pin(idx),
            Some(KeyOwner::View(idx)) => {
                if let Some(q) = self.pins.views.get(idx).map(|v| v.query.clone()) {
                    self.open_pin_picker(&format!("{q} "));
                }
            }
            None if self.pins_loaded && k.len() == 1 => {
                self.set_status(format!("no pin on '{k} (m{k} pins this folder)"), false)
            }
            None if self.pins_loaded => self.set_status(format!("no pin on '{k}"), false),
            None => self.set_status("pins are still loading".into(), false),
        }
    }

    /// Go to a folder pin, open a file pin. Other kinds run from the launcher (M5) or
    /// as jobs (M4b).
    pub(super) fn open_pin(&mut self, idx: usize) {
        let Some(pin) = self.pins.pins.get(idx).cloned() else {
            return;
        };
        // Frecency: whatever opens a pin counts (launcher, picker, 'key, Locations).
        self.record_use(&pin);
        match pin.kind {
            PinKind::Machine => return self.open_machine_pin(&pin),
            PinKind::Sync => return self.open_sync_pin(idx, false),
            PinKind::Backup => return self.open_backup_pin(idx),
            PinKind::App | PinKind::Web => {
                return match crate::launcher::launch_of(&pin) {
                    Some(l) => self.worker.launch(format!("started {}", pin.name), l),
                    None => self.set_status(
                        format!("{} needs a command or url (^e edits it)", pin.name),
                        true,
                    ),
                };
            }
            _ => {}
        }
        let Some(target) = pin.target() else {
            self.set_status(format!("{}: no valid path (^e edits it)", pin.name), true);
            return;
        };
        match pin.kind {
            PinKind::Folder => {
                // Known unreachable: say so and stay put, and check again for next time.
                if let Some(Err(e)) = self.reach.get(&target).and_then(|r| r.last.clone()) {
                    return self.stay_put(&pin.name, target, &e);
                }
                self.navigate(target, None);
            }
            _ => {
                self.set_status(format!("opening {}…", pin.name), false);
                self.worker.open(target);
            }
        }
    }

    /// Counts one use of `pin` for frecency, saved through the worker.
    pub(super) fn record_use(&mut self, pin: &Pin) {
        if let Some(key) = crate::frecency::pin_key(pin) {
            self.usage.record(&key, crate::frecency::now_secs());
            self.worker.save_usage(self.usage.clone());
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
            key: pin.key_str().map(str::to_string),
            fields: self.preset_fields(&pin),
            pin,
            editing: Some(idx),
            ask_key: true,
            single: false,
            reopen,
            name_last: false,
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
    pub(super) fn pin_machine_draft(&mut self, pin: Pin, key: Option<String>) {
        let ask_key = key.is_none();
        self.start_draft(PinDraft::new(pin, key, ask_key));
    }

    /// `^a` on a file or folder row in a picker: a file / folder pin for it.
    pub(super) fn pin_loc(&mut self, loc: &Loc, is_dir: bool) {
        if !self.pins_ready() {
            return;
        }
        let kind = PinKind::for_item(&loc.name(), is_dir);
        let pin = Pin::for_loc(kind, &loc.name(), loc);
        self.start_draft(PinDraft::new(pin, None, true));
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
        if kind == PromptKind::KeyMove {
            return self.key_move_submit(text);
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
                Some(
                    d.fields
                        .first()
                        .map_or(PromptKind::PinTags, |f| PromptKind::Preset(*f)),
                )
            }
            PromptKind::Preset(f) => {
                if let Err(e) = self.set_preset_field(&mut d.pin, f, text) {
                    self.pin_draft = Some(d);
                    return Err(e);
                }
                d.fields.retain(|x| *x != f);
                match d.fields.first() {
                    Some(next) => Some(PromptKind::Preset(*next)),
                    None => {
                        // The whole preset, as `sentier run` and the loader will see it.
                        if let Err(e) = check_preset(&d.pin) {
                            d.fields.insert(0, f);
                            self.pin_draft = Some(d);
                            return Err(e);
                        }
                        if std::mem::take(&mut d.name_last) {
                            d.pin.name = suggested_name(&d.pin);
                            Some(PromptKind::PinName)
                        } else {
                            Some(PromptKind::PinTags)
                        }
                    }
                }
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
            PromptKind::PinKey => match pins::parse_key(text) {
                Ok(k) => {
                    d.key = k;
                    None
                }
                Err(e) => {
                    self.pin_draft = Some(d);
                    return Err(e);
                }
            },
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
                    PromptKind::PinKey => d.key.clone().unwrap_or_default(),
                    PromptKind::Preset(f) => preset_value(&d.pin, f),
                    PromptKind::PinName => d.pin.name.clone(),
                    _ => String::new(),
                };
                // A folder field: go there and press p, rather than type it.
                if let PromptKind::Preset(f) = next
                    && super::pick::is_pick_field(f)
                {
                    let start = Loc::parse(&prefill).ok().filter(|l| l.as_fs().is_some());
                    self.pin_draft = Some(d);
                    self.start_pick(f, start);
                    return Ok(());
                }
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
        if kind == PromptKind::KeyMove {
            return self.key_move_cancel();
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
            Some(Reopen::Launcher(q)) => self.open_launcher(&q),
            Some(Reopen::Tags) => self.open_tag_picker(),
            Some(Reopen::Machines(q)) => self.open_machine_picker(&q),
            None => {}
        }
    }

    /// The last step of a draft: save it, once its key clashes with nothing.
    fn finish_draft(&mut self, d: PinDraft) {
        let gives_key = d.ask_key || d.editing.is_none();
        match d.key.clone().filter(|_| gives_key) {
            Some(k) => {
                let name = d.pin.name.clone();
                let except = d.editing.map(KeyOwner::Pin);
                self.give_key(k, name, except, KeyThen::Draft(Box::new(d)));
            }
            None => self.save_draft(d),
        }
    }

    fn save_draft(&mut self, d: PinDraft) {
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
        let gives_key = d.ask_key || !edited;
        let moved = if gives_key {
            self.pins.set_key(idx, d.key.as_deref())
        } else {
            None
        };
        self.save_pins();
        let msg = match (d.key.as_deref(), edited) {
            (Some(k), _) if gives_key => pinned_message(k, &name, moved.as_deref()),
            (_, true) => format!("saved {name}"),
            (_, false) => format!("pinned {name}"),
        };
        self.set_status(msg, false);
        self.reopen_picker(d.reopen);
    }

    // ------------------------------------------------------------ key clashes

    /// Gives key `k` to `name` (already in the store as `except`, or about to be) and
    /// then finishes `then`. Keys it clashes with (`'d` for `'dd`) are moved first, each
    /// through a prompt; nothing changes until the last one is answered.
    fn give_key(&mut self, k: String, name: String, except: Option<KeyOwner>, then: KeyThen) {
        let queue = self.pins.key_clashes(&k, except);
        if queue.is_empty() {
            return self.key_then(then, &k);
        }
        self.key_move = Some(KeyMove {
            key: k,
            name,
            except,
            queue,
            moves: Vec::new(),
            then,
        });
        self.ask_key_move();
    }

    fn ask_key_move(&mut self) {
        let Some(m) = &self.key_move else { return };
        let Some(&owner) = m.queue.first() else {
            return;
        };
        let suggestion = self.suggest_key(m, owner).unwrap_or_default();
        self.open_prompt_with(PromptKind::KeyMove, &suggestion);
    }

    /// The note under the move prompt: why it's asked, or what's wrong with the answer.
    pub(super) fn key_move_note(&self, text: &str) -> String {
        let Some(m) = &self.key_move else {
            return String::new();
        };
        let Some(&owner) = m.queue.first() else {
            return String::new();
        };
        let why = format!(
            "'{} ({}) blocks '{} for {}: a key can't also start longer keys. Its new key, empty = none",
            self.pins.owner_key(owner).unwrap_or_default(),
            self.pins.owner_name(owner).unwrap_or_default(),
            m.key,
            m.name
        );
        match pins::parse_key(text).and_then(|k| self.check_move(m, owner, k.as_deref())) {
            Ok(()) => why,
            Err(e) => e,
        }
    }

    /// The keys there will be once this move is done, but `owner`'s: (key, whose).
    fn keys_after(&self, m: &KeyMove, owner: KeyOwner) -> Vec<(String, String)> {
        let name = |o| self.pins.owner_name(o).unwrap_or_default().to_string();
        let mut out: Vec<(String, String)> = self
            .pins
            .keyed()
            .filter(|(o, k)| {
                *o != owner
                    && Some(*o) != m.except
                    && *k != m.key
                    && !m.queue.contains(o)
                    && !m.moves.iter().any(|(mo, _)| mo == o)
            })
            .map(|(o, k)| (k.to_string(), name(o)))
            .collect();
        out.extend(
            m.moves
                .iter()
                .filter_map(|(o, k)| Some((k.clone()?, name(*o)))),
        );
        out.push((m.key.clone(), m.name.clone()));
        out
    }

    fn check_move(&self, m: &KeyMove, owner: KeyOwner, k: Option<&str>) -> Result<(), String> {
        let Some(k) = k else { return Ok(()) };
        for (other, whose) in self.keys_after(m, owner) {
            if other == k {
                return Err(format!("'{k} is {whose}"));
            }
            if pins::keys_clash(k, &other) {
                return Err(format!("'{k} clashes with '{other} ({whose})"));
            }
        }
        Ok(())
    }

    /// A free key for `owner`: its old letter plus a letter of its name (`'d` → `'dr`),
    /// a letter of its name, or any free one.
    fn suggest_key(&self, m: &KeyMove, owner: KeyOwner) -> Option<String> {
        let old = self.pins.owner_key(owner).unwrap_or_default();
        let letters: Vec<char> = self
            .pins
            .owner_name(owner)
            .unwrap_or_default()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let any: Vec<char> = ('a'..='z').chain('0'..='9').collect();
        let pool: Vec<char> = letters.iter().chain(&any).copied().collect();
        let mut candidates: Vec<String> = Vec::new();
        if let Some(f) = old.chars().next() {
            candidates.extend(pool.iter().map(|l| format!("{f}{l}")));
        }
        candidates.extend(pool.iter().map(char::to_string));
        if let Some(f) = letters.first() {
            candidates.extend(pool.iter().map(|l| format!("{f}{l}")));
        }
        candidates
            .into_iter()
            .find(|k| k != old && self.check_move(m, owner, Some(k)).is_ok())
    }

    fn key_move_submit(&mut self, text: &str) -> Result<(), String> {
        let Some(mut m) = self.key_move.take() else {
            return Ok(());
        };
        let Some(&owner) = m.queue.first() else {
            return Ok(());
        };
        let k = pins::parse_key(text).and_then(|k| {
            self.check_move(&m, owner, k.as_deref())?;
            Ok(k)
        });
        let k = match k {
            Ok(k) => k,
            Err(e) => {
                self.key_move = Some(m);
                return Err(e);
            }
        };
        m.queue.remove(0);
        m.moves.push((owner, k));
        if !m.queue.is_empty() {
            self.key_move = Some(m);
            self.ask_key_move();
            return Ok(());
        }
        for (o, k) in &m.moves {
            self.pins.put_key(*o, k.as_deref());
        }
        let moved: Vec<String> = m
            .moves
            .iter()
            .map(|(o, k)| {
                let who = self.pins.owner_name(*o).unwrap_or_default();
                match k {
                    Some(k) => format!("{who} → '{k}"),
                    None => format!("{who}: no key"),
                }
            })
            .collect();
        self.key_then(m.then, &m.key);
        // After the "'dd → Desktop" message, say what moved for it.
        if let Some(s) = self.status.as_mut() {
            s.text = format!("{} · {}", s.text, moved.join(" · "));
        }
        Ok(())
    }

    fn key_move_cancel(&mut self) {
        let Some(m) = self.key_move.take() else {
            return;
        };
        let reopen = match m.then {
            KeyThen::Draft(d) => {
                let msg = if d.editing.is_some() {
                    "unchanged"
                } else {
                    "not pinned"
                };
                self.set_status(msg.to_string(), false);
                d.reopen
            }
            KeyThen::View(d) => {
                self.set_status("view key unchanged".into(), false);
                d.reopen()
            }
            KeyThen::Mark(_) => {
                self.set_status("unchanged".into(), false);
                None
            }
        };
        self.reopen_picker(reopen);
    }

    /// Nothing clashes any more: give the key and save.
    fn key_then(&mut self, then: KeyThen, k: &str) {
        match then {
            KeyThen::Draft(d) => self.save_draft(*d),
            KeyThen::Mark(idx) => {
                let moved = self.pins.set_key(idx, Some(k));
                let name = self.pins.pins.get(idx).map(|p| p.name.clone());
                self.save_pins();
                if let Some(name) = name {
                    self.set_status(pinned_message(k, &name, moved.as_deref()), false);
                }
            }
            KeyThen::View(d) => self.finish_view_key(d, Some(k.to_string())),
        }
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

// ------------------------------------------------------------------ presets

impl App {
    /// The preset fields a pin's prompts ask for. The 7-Zip engine is only offered when
    /// 7-Zip is installed (or the pin already names an engine).
    fn preset_fields(&self, pin: &Pin) -> Vec<PresetField> {
        use PresetField::*;
        match pin.kind {
            PinKind::Sync => vec![Source, Dest, Mode],
            PinKind::Backup => {
                let mut v = vec![Source, DestDir, FileName, Keep];
                if self.seven_zip.is_some() || pin.extra.contains_key("engine") {
                    v.push(Engine);
                }
                v
            }
            PinKind::Web => vec![Url, Browser],
            PinKind::App => vec![Command, Args, Dir],
            _ => Vec::new(),
        }
    }

    /// `:pin url [URL]`, `^a` in the launcher: a web app pin (name → url → browser →
    /// tags → key). The name starts as the URL's host.
    pub(super) fn new_web_pin(&mut self, url: &str) {
        if !self.pins_ready() {
            return;
        }
        let url = crate::launcher::normalize_url(url).unwrap_or_default();
        let host = url
            .split("://")
            .nth(1)
            .and_then(|r| r.split(['/', '?', '#']).next())
            .unwrap_or_default()
            .trim_start_matches("www.")
            .to_string();
        let mut pin = Pin::blank(PinKind::Web, &host);
        pin.url = (!url.is_empty()).then_some(url);
        let mut d = PinDraft::new(pin, None, true);
        d.fields = self.preset_fields(&d.pin);
        self.start_draft(d);
    }

    /// `^a` on a Start Menu app in the launcher: an app pin for its shortcut.
    pub(super) fn new_app_pin(&mut self, name: &str, command: &str) {
        if !self.pins_ready() {
            return;
        }
        let mut pin = Pin::blank(PinKind::App, name);
        pin.command = Some(command.to_string());
        let mut d = PinDraft::new(pin, None, true);
        d.fields = self.preset_fields(&d.pin);
        self.start_draft(d);
    }

    /// `Space n s` / `:sync new` / `Space n b` / `:backup new`: a preset whose source is the
    /// folder you're in (for a sync, only what's selected there, if anything). Then you
    /// go to the destination (pick mode, `p`), answer the short questions (mode | keep,
    /// engine), and name it last: name → tags → key.
    pub(super) fn new_preset(&mut self, kind: PinKind) {
        if !self.pins_ready() {
            return;
        }
        if self.pick.is_some() {
            return self.set_status(
                "finish the folder you're picking first (p, or Esc)".into(),
                false,
            );
        }
        let Some(src) = self.cwd.real().as_fs().cloned() else {
            return self.set_status(format!("go to the folder to {} first", kind.label()), true);
        };
        let selected = self.selected_for_include();
        let mut pin = Pin::blank(kind, &src.name());
        pin.extra
            .insert("source".into(), toml::Value::String(src.display()));
        let mut note = None;
        match kind {
            PinKind::Sync => {
                pin.extra
                    .insert("mode".into(), toml::Value::String("update".into()));
                if !selected.is_empty() {
                    pin.extra.insert(
                        "include".into(),
                        toml::Value::Array(selected.into_iter().map(toml::Value::String).collect()),
                    );
                }
            }
            _ => {
                pin.extra.insert("keep".into(), toml::Value::Integer(10));
                if !selected.is_empty() {
                    note = Some("a backup zips the whole folder (the selection is for syncs)");
                }
            }
        }
        self.visual = None;
        self.marked.clear();
        let mut d = PinDraft::new(pin, None, true);
        // The source is where you are.
        d.fields = self
            .preset_fields(&d.pin)
            .into_iter()
            .filter(|f| *f != PresetField::Source)
            .collect();
        d.name_last = true;
        let first = d.fields.first().copied();
        self.pin_draft = Some(d);
        match first {
            Some(f) if super::pick::is_pick_field(f) => self.start_pick(f, None),
            Some(f) => {
                let prefill = self
                    .pin_draft
                    .as_ref()
                    .map(|d| preset_value(&d.pin, f))
                    .unwrap_or_default();
                self.open_prompt_with(PromptKind::Preset(f), &prefill);
            }
            None => {}
        }
        if let Some(n) = note {
            self.set_status(n.into(), false);
        }
    }

    /// The selected (or visual-range) items here, as `include` names: `DEMO\`, `a.txt`.
    fn selected_for_include(&self) -> Vec<String> {
        if self.visual.is_none() && self.marked.is_empty() {
            return Vec::new();
        }
        let Some(listing) = self.view_listing.as_ref() else {
            return Vec::new();
        };
        self.operand(1)
            .into_iter()
            .filter_map(|i| listing.entries.get(i))
            .filter(|e| matches!(e.kind, EntryKind::File | EntryKind::Dir))
            .map(|e| {
                if e.kind == EntryKind::Dir {
                    format!("{}\\", e.name)
                } else {
                    e.name.clone()
                }
            })
            .collect()
    }
    /// A folder typed in a preset field: `~`, relative to the cwd, any path form.
    fn preset_path(&self, text: &str) -> Result<crate::fs::path::FsPath, String> {
        let t = super::keys::expand_home(text)?;
        if t.trim().is_empty() {
            return Err("a folder path, please (Tab completes)".into());
        }
        match Loc::resolve(&self.cwd, t.trim()) {
            Ok(Loc::Fs(p)) => Ok(p),
            Ok(_) => Err("a real folder (C:\\…, \\\\server\\share\\…), not a virtual one".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Checks one field and writes it into the pin. Err keeps the prompt open.
    fn set_preset_field(&self, pin: &mut Pin, f: PresetField, text: &str) -> Result<(), String> {
        use crate::jobs::preset::check_paths;
        let opt = |t: &str| (!t.trim().is_empty()).then(|| t.trim().to_string());
        // App and web fields live on the pin itself.
        match f {
            PresetField::Url => {
                pin.url =
                    Some(crate::launcher::normalize_url(text).ok_or("a web address (https://…)")?);
                return Ok(());
            }
            PresetField::Browser => {
                let b = text.trim();
                pin.browser = match b.to_ascii_lowercase().as_str() {
                    "" | "edge" | "msedge" => None,
                    "chrome" | "brave" | "vivaldi" => Some(b.to_ascii_lowercase()),
                    _ if b.contains(['\\', '/']) => Some(b.to_string()),
                    other => {
                        return Err(format!(
                            "{other:?}: edge, chrome, brave, vivaldi or a path to the browser"
                        ));
                    }
                };
                return Ok(());
            }
            PresetField::Command => {
                pin.command = Some(opt(text).ok_or("the program, .bat, .lnk or file to run")?);
                return Ok(());
            }
            PresetField::Args => {
                pin.args = opt(text);
                return Ok(());
            }
            PresetField::Dir => {
                pin.dir = opt(text);
                return Ok(());
            }
            _ => {}
        }
        let source = || {
            pin.extra
                .get("source")
                .and_then(|v| v.as_str())
                .and_then(|s| self.preset_path(s).ok())
        };
        let (key, value) = match f {
            PresetField::Source => (
                "source",
                toml::Value::String(self.preset_path(text)?.display()),
            ),
            PresetField::Dest => {
                let dest = self.preset_path(text)?;
                if let Some(src) = source() {
                    check_paths(&src, &dest)?;
                }
                ("dest", toml::Value::String(dest.display()))
            }
            PresetField::DestDir => {
                let dir = self.preset_path(text)?;
                if let Some(src) = source()
                    && let Err(e) = check_paths(&src, &dir)
                    && !e.contains("source is inside dest")
                {
                    return Err(format!(
                        "{} (zips can't go into the folder they back up)",
                        e.replace("dest", "the zips folder")
                    ));
                }
                ("dest_dir", toml::Value::String(dir.display()))
            }
            PresetField::Mode => {
                let m = match text.trim().to_ascii_lowercase().as_str() {
                    "u" | "update" | "" => "update",
                    "m" | "mirror" => "mirror",
                    other => return Err(format!("{other:?}: update or mirror")),
                };
                ("mode", toml::Value::String(m.into()))
            }
            PresetField::FileName => {
                let t = match text.trim() {
                    "" => crate::jobs::zip::DEFAULT_FILE_NAME,
                    t => t,
                };
                self.zip_name_preview(pin, t)?;
                ("file_name", toml::Value::String(t.to_string()))
            }
            PresetField::Keep => match text.trim().parse::<i64>() {
                Ok(n) if (0..=10_000).contains(&n) => ("keep", toml::Value::Integer(n)),
                _ => return Err("a number: how many zips to keep (0 keeps all)".into()),
            },
            PresetField::Engine => {
                let e = crate::jobs::zip::Engine::parse(text)?;
                ("engine", toml::Value::String(e.word().into()))
            }
            // Handled above.
            PresetField::Url
            | PresetField::Browser
            | PresetField::Command
            | PresetField::Args
            | PresetField::Dir => return Ok(()),
        };
        pin.extra.insert(key.into(), value);
        Ok(())
    }

    /// The zip a backup preset would write now with name template `t`
    /// (`HIT_2026-11-10_1430.zip`), or what's wrong with `t`.
    fn zip_name_preview(&self, pin: &Pin, t: &str) -> Result<String, String> {
        let source = pin
            .extra
            .get("source")
            .and_then(|v| v.as_str())
            .and_then(|s| self.preset_path(s).ok())
            .map(|p| p.name())
            .unwrap_or_default();
        let vars = crate::jobs::zip::NameVars::here(&source);
        crate::jobs::zip::expand_name(t, &vars, chrono::Local::now())
            .map_err(|e| e.replace("file_name: ", "").replace("file_name ", ""))
    }

    /// The line under a preset field: what it's for.
    pub(super) fn preset_note(&self, f: PresetField, text: &str) -> String {
        match f {
            PresetField::Source => "the folder to copy from · Tab completes".into(),
            PresetField::Dest => "the local working copy; never inside the source · Tab completes".into(),
            PresetField::Mode if text.trim().to_ascii_lowercase().starts_with('m') => {
                "mirror: the local copy becomes exact; local files not on the server are DELETED (always previewed)".into()
            }
            PresetField::Mode => "update: copies new and changed files, never deletes · mirror: exact copy".into(),
            PresetField::DestDir => "folder for the zips, local or \\\\server\\share · Tab completes".into(),
            PresetField::FileName => {
                let pin = self.pin_draft.as_ref().map(|d| &d.pin);
                let t = match text.trim() {
                    "" => crate::jobs::zip::DEFAULT_FILE_NAME,
                    t => t,
                };
                match pin.map(|p| self.zip_name_preview(p, t)) {
                    Some(Ok(name)) => {
                        format!("→ {name} · tokens: {}", crate::jobs::zip::NAME_TOKENS)
                    }
                    Some(Err(e)) => e,
                    None => String::new(),
                }
            }
            PresetField::Keep => "older zips of this preset beyond that are deleted after a good backup".into(),
            PresetField::Engine => match &self.seven_zip {
                Some(p) => format!("7zip: faster, multi-threaded ({p}) · builtin: always works"),
                None => "7-Zip isn't installed here: builtin is used either way".into(),
            },
            PresetField::Url => "opens as its own window (--app), like an installed app".into(),
            PresetField::Browser => "empty = Edge (always on Windows 11)".into(),
            PresetField::Command => {
                "an .exe, .bat, .lnk or document; %VARS% are expanded when it runs".into()
            }
            PresetField::Args => "passed as typed".into(),
            PresetField::Dir => "the working folder · Tab completes".into(),
        }
    }
}

/// A preset field's current value, to prefill its prompt.
fn preset_value(pin: &Pin, f: PresetField) -> String {
    let get = |k: &str| pin.extra.get(k);
    let s = |k: &str| {
        get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    match f {
        PresetField::Source => s("source"),
        PresetField::Dest => s("dest"),
        PresetField::DestDir => s("dest_dir"),
        PresetField::FileName => get("file_name")
            .and_then(|v| v.as_str())
            .unwrap_or(crate::jobs::zip::DEFAULT_FILE_NAME)
            .into(),
        PresetField::Mode => get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("update")
            .into(),
        PresetField::Keep => get("keep")
            .and_then(|v| v.as_integer())
            .unwrap_or(0)
            .to_string(),
        PresetField::Engine => get("engine")
            .and_then(|v| v.as_str())
            .unwrap_or("builtin")
            .into(),
        PresetField::Url => pin.url.clone().unwrap_or_default(),
        PresetField::Browser => pin.browser.clone().unwrap_or_else(|| "edge".into()),
        PresetField::Command => pin.command.clone().unwrap_or_default(),
        PresetField::Args => pin.args.clone().unwrap_or_default(),
        PresetField::Dir => pin.dir.clone().unwrap_or_default(),
    }
}

/// The whole preset (or app / web pin), checked as the loader and `sentier run` will.
fn check_preset(pin: &Pin) -> Result<(), String> {
    match pin.kind {
        PinKind::Sync => crate::jobs::preset::SyncPreset::from_pin(pin).map(|_| ()),
        PinKind::Backup => crate::jobs::zip::BackupPreset::from_pin(pin).map(|_| ()),
        PinKind::Web if pin.url.is_none() => Err("a web app needs a url".into()),
        PinKind::App if pin.command.is_none() => Err("an app needs a command".into()),
        _ => Ok(()),
    }
}

fn last_word(text: &str) -> &str {
    text.rsplit([' ', ',']).next().unwrap_or_default()
}

fn pinned_message(c: &str, name: &str, moved: Option<&str>) -> String {
    match moved {
        Some(old) if old != name => format!("'{c} → {name} (was {old})"),
        _ => format!("'{c} → {name}"),
    }
}

/// A new preset's name from its folders: `HIT → HIT-local`, `HIT backup`.
fn suggested_name(pin: &Pin) -> String {
    let last = |k: &str| {
        pin.extra.get(k).and_then(|v| v.as_str()).map(|s| {
            s.trim_end_matches('\\')
                .rsplit('\\')
                .next()
                .unwrap_or(s)
                .to_string()
        })
    };
    match (pin.kind, last("source"), last("dest")) {
        (PinKind::Sync, Some(s), Some(d)) => format!("{s} → {d}"),
        (PinKind::Backup, Some(s), _) => format!("{s} backup"),
        _ => pin.name.clone(),
    }
}
