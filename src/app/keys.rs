//! Key dispatch, the actions bindings run, and the prompts.
//!
//! Normal mode is driven by [`crate::keymap::Keymap`]: keys accumulate until they match a
//! binding. A pure prefix (`g`, `<leader>`) waits, and after the keymap timeout the
//! which-key popup lists what can follow. A binding that is also a prefix of a longer one
//! runs when the timeout passes, as in vim. Digits before a command are a count.
//!
//! Prompts (`:` command line, `/` `?` search, `f` filter) are a [`LineEditor`] plus
//! what the prompt does with each change.

use super::{App, Mode, Quit, Search};
use crate::fs::EntryKind;
use crate::fs::path::{Loc, split_for_completion};
use crate::keymap::{Action, Key, Lookup};
use crate::prompt::{Edit, LineEditor};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

/// Most a count can repeat a navigation (`50h` shouldn't queue 50 listings).
const MAX_NAV_REPEAT: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Command,
    Search {
        backward: bool,
    },
    Filter,
    /// `s`: recursive name search (runs on Enter).
    NameSearch,
    /// `S`: recursive content search (runs on Enter).
    ContentSearch,
    /// Pin prompts (`m`, `M`, `r`/`T`/`dd` on a pin): name → tags → key.
    PinName,
    PinTags,
    PinKey,
    /// `y` unpins, any other key keeps the pin.
    ConfirmUnpin,
    /// A y/n for a store change (tag rename/delete, view delete).
    Confirm,
    /// Saved views: rename, then key.
    ViewName,
    ViewKey,
    /// The tag picker's `^e`: the tag's new name.
    TagRename,
    /// `gP` without a count: which PDF page.
    GotoPage,
    /// `u` in a machine view: whose folders the templates open.
    MachineUser,
    /// `r` / `cw`: the item's new name.
    Rename,
    /// `a` / `A`: a new file / folder's name.
    NewFile,
    NewDir,
    /// One field of a sync / backup preset (`:sync new`, `:backup new`, `^e`).
    Preset(PresetField),
}

/// The fields a preset prompt asks for, between the name and the tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetField {
    Source,
    /// A sync's local copy.
    Dest,
    Mode,
    /// Where a backup's zips go.
    DestDir,
    Keep,
    Engine,
    /// A web pin's address and browser.
    Url,
    Browser,
    /// An app pin's program, its arguments and the folder it runs in.
    Command,
    Args,
    Dir,
}

impl PresetField {
    pub fn prefix(self) -> &'static str {
        match self {
            PresetField::Source => "source: ",
            PresetField::Dest => "local copy (dest): ",
            PresetField::Mode => "mode (update | mirror): ",
            PresetField::DestDir => "zips go to: ",
            PresetField::Keep => "keep newest (0 = all): ",
            PresetField::Engine => "zip engine (builtin | 7zip): ",
            PresetField::Url => "url: ",
            PresetField::Browser => "browser (edge | chrome | brave | vivaldi | path): ",
            PresetField::Command => "command: ",
            PresetField::Args => "arguments (empty = none): ",
            PresetField::Dir => "start in (empty = its folder): ",
        }
    }

    /// Fields that take a folder (Tab completes them).
    pub fn is_path(self) -> bool {
        matches!(
            self,
            PresetField::Source | PresetField::Dest | PresetField::DestDir | PresetField::Dir
        )
    }
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    pub editor: LineEditor,
    /// A one-line note after the prompt: completion matches, "no match".
    pub note: String,
    /// The cursor row when the prompt opened (a cancelled search goes back there).
    origin: usize,
    /// The search before this one, restored when the search is cancelled.
    saved_search: Option<Search>,
    completion: Option<Completion>,
    /// A yes/no popup: the button Enter presses (`h`/`l`/Tab switch).
    pub yes: bool,
}

impl Prompt {
    pub fn prefix(&self) -> &'static str {
        match self.kind {
            PromptKind::Command => ":",
            PromptKind::Search { backward: false } => "/",
            PromptKind::Search { backward: true } => "?",
            PromptKind::Filter => "filter: ",
            PromptKind::NameSearch => "search: ",
            PromptKind::ContentSearch => "grep: ",
            PromptKind::PinName => "pin name: ",
            PromptKind::PinTags => "tags: ",
            PromptKind::PinKey => "key (empty = none): ",
            PromptKind::ConfirmUnpin | PromptKind::Confirm => "",
            PromptKind::ViewName => "view name: ",
            PromptKind::ViewKey => "view key (empty = none): ",
            PromptKind::TagRename => "rename tag to: ",
            PromptKind::GotoPage => "go to page: ",
            PromptKind::MachineUser => "templates for user: ",
            PromptKind::Rename => "rename to: ",
            PromptKind::NewFile => "new file: ",
            PromptKind::NewDir => "new folder: ",
            PromptKind::Preset(f) => f.prefix(),
        }
    }
}

/// Tab completion of the path argument of `:cd` / `:e`, from the listing cache. A
/// directory that isn't cached is listed through the worker like any other, and the
/// completion fills in when it arrives.
#[derive(Debug, Clone)]
struct Completion {
    /// The line before the path argument, e.g. `cd `.
    head: String,
    /// The directory part as typed (`C:\wo` → `C:\`), kept verbatim in the result.
    dir_text: String,
    dir: Loc,
    prefix: String,
    candidates: Vec<String>,
    loaded: bool,
    idx: usize,
    /// The line as completion last wrote it. Any other edit starts a new completion.
    applied: String,
}

fn home() -> Result<String, String> {
    std::env::var("USERPROFILE").map_err(|_| "USERPROFILE is not set".to_string())
}

/// `~` and `~\rest` → the home directory (input sugar for `:cd` and bindings).
/// The line under `:`: the commands matching what's typed, with what they do.
fn command_note(text: &str) -> String {
    if text.trim().is_empty() {
        let mut heads: Vec<&str> = crate::keymap::COMMANDS
            .iter()
            .map(|(c, _, _)| c.split(' ').next().unwrap_or(c))
            .collect();
        heads.dedup();
        return format!("{} · g? lists them all", heads.join(" · "));
    }
    let hints = crate::keymap::command_hints(text);
    if hints.is_empty() {
        "no such command · g? lists them all".to_string()
    } else {
        hints.join("   ·   ")
    }
}

pub(super) fn expand_home(text: &str) -> Result<String, String> {
    let text = text.trim();
    match text.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with(['\\', '/']) => Ok(format!("{}{rest}", home()?)),
        _ => Ok(text.to_string()),
    }
}

impl App {
    pub(super) fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = Some(Quit::Plain);
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Normal,
            Mode::Prompt => self.on_prompt_key(key),
            Mode::Picker => self.on_picker_key(key),
            Mode::Preview => self.on_preview_key(key),
            Mode::Credentials => self.on_credentials_key(key),
            Mode::NetList => self.on_net_list_key(key),
            Mode::Jobs => self.on_jobs_key(key),
            Mode::SyncPreview => self.on_sync_preview_key(key),
            Mode::OpenWith => self.on_open_menu_key(key),
            Mode::Normal => self.on_normal_key(key),
        }
    }

    // ---------------------------------------------------------------- normal mode

    fn on_normal_key(&mut self, ev: KeyEvent) {
        if self.visual_key(&ev) {
            return;
        }
        let key = Key::from_event(&ev);
        // In a machine view a template's key jumps there, and `u` picks the user.
        if self.keys_pending.is_empty()
            && self.count.is_none()
            && let Some(c) = key.plain_char()
            && self.machine_key(c)
        {
            return;
        }
        if self.keys_pending.is_empty() {
            // `0` starts nothing on its own; after a digit it's part of the count.
            if let Some(d) = key.is_digit()
                && (d != 0 || self.count.is_some())
            {
                let c = self.count.unwrap_or(0).saturating_mul(10) + d as usize;
                self.count = Some(c.min(999_999));
                return;
            }
        } else {
            match key.code {
                KeyCode::Esc => {
                    self.clear_keys();
                    return;
                }
                // which-key: back up a level.
                KeyCode::Backspace => {
                    self.keys_pending.pop();
                    if self.keys_pending.is_empty() {
                        self.clear_keys();
                    }
                    return;
                }
                _ => {}
            }
        }
        if self.keys_pending.is_empty() && key.code == KeyCode::Esc && self.count.is_some() {
            self.count = None;
            return;
        }
        self.keys_pending.push(key);
        let found = match self.keymap.lookup(&self.keys_pending) {
            Lookup::None => None,
            Lookup::Prefix => Some(None),
            Lookup::Exact { action, longer } => Some((!longer).then(|| action.clone())),
        };
        match found {
            None => {
                if self.keys_pending.len() > 1 {
                    let seq = crate::keymap::fmt_keys(&self.keys_pending, self.keymap.leader);
                    self.set_status(format!("{seq}: no mapping"), false);
                }
                self.clear_keys();
            }
            // Wait for more keys (or the timeout).
            Some(None) => {
                self.keys_since.get_or_insert_with(Instant::now);
            }
            Some(Some(action)) => {
                let (action, count) = (action.with_char(&key), self.count.take());
                self.clear_keys();
                self.run(action, count);
            }
        }
    }

    fn clear_keys(&mut self) {
        self.keys_pending.clear();
        self.keys_since = None;
        self.which_key = false;
        self.count = None;
    }

    /// From `tick`: once the timeout passes, run a binding that was waiting on a longer
    /// one, or open the which-key popup for a pure prefix.
    pub(super) fn key_timeout(&mut self, now: Instant) {
        let Some(t) = self.keys_since else { return };
        if self.which_key || now.duration_since(t) < self.keymap.timeout {
            return;
        }
        if let Lookup::Exact { action, .. } = self.keymap.lookup(&self.keys_pending) {
            let last = self
                .keys_pending
                .last()
                .copied()
                .unwrap_or(crate::keymap::ANY_CHAR);
            let (action, count) = (action.clone().with_char(&last), self.count.take());
            self.clear_keys();
            self.run(action, count);
        } else {
            self.which_key = true;
            self.dirty = true;
        }
    }

    // -------------------------------------------------------------------- actions

    pub(super) fn run(&mut self, action: Action, count: Option<usize>) {
        let n = count.unwrap_or(1).max(1);
        let ni = n.min(isize::MAX as usize) as isize;
        let half = (self.page / 2).max(1) as isize;
        let page = self.page.max(1) as isize;
        match action {
            Action::Down => self.move_cursor(ni),
            Action::Up => self.move_cursor(-ni),
            Action::Parent => {
                for _ in 0..n.min(MAX_NAV_REPEAT) {
                    self.go_parent();
                }
            }
            Action::Open => self.open_selected(),
            Action::Top => self.set_cursor(count.map_or(0, |c| c.saturating_sub(1))),
            Action::Bottom => self.set_cursor(count.map_or(usize::MAX, |c| c.saturating_sub(1))),
            Action::HalfPageDown => self.move_cursor(half.saturating_mul(ni)),
            Action::HalfPageUp => self.move_cursor(-half.saturating_mul(ni)),
            Action::PageDown => self.move_cursor(page.saturating_mul(ni)),
            Action::PageUp => self.move_cursor(-page.saturating_mul(ni)),
            // The UI's scroll-off clamp turns these into vim's exact positions.
            Action::ScrollMiddle => self.offset = self.cursor.saturating_sub(self.page / 2),
            Action::ScrollTop => self.offset = self.cursor,
            Action::ScrollBottom => self.offset = (self.cursor + 1).saturating_sub(self.page),
            Action::SearchForward => self.open_prompt(PromptKind::Search { backward: false }),
            Action::SearchBackward => self.open_prompt(PromptKind::Search { backward: true }),
            Action::SearchNext => self.search_again(n, false),
            Action::SearchPrev => self.search_again(n, true),
            Action::Filter => self.open_prompt(PromptKind::Filter),
            Action::Escape if self.clear_marks() => {}
            Action::Escape if self.filter.is_empty() && self.results_escape() => {}
            Action::Escape => {
                self.search_hl = false;
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_view();
                }
            }
            Action::Back => self.history(n, true),
            Action::Forward => self.history(n, false),
            Action::PreviousDir => match self.prev_dir.clone() {
                Some(p) => self.navigate(p, None),
                None => self.set_status("no previous directory".into(), false),
            },
            Action::Locations => self.navigate(Loc::Root, None),
            Action::Downloads => self.cd("~\\Downloads"),
            Action::VolumeRoot => match self.cwd.as_fs().map(|p| Loc::Fs(p.volume_root())) {
                Some(root) => self.navigate(root, None),
                None => self.set_status("not on a drive or share".into(), false),
            },
            Action::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.rebuild_view();
                let state = if self.show_hidden { "shown" } else { "hidden" };
                self.set_status(format!("hidden/system files {state}"), false);
            }
            Action::Refresh => self.refresh(),
            Action::Delete
                if self.at_root() && self.selected().is_some_and(|e| e.kind == EntryKind::Pin) =>
            {
                self.unpin_selected()
            }
            Action::Delete if self.at_root() => self.forget_selected_share(),
            Action::Delete => self.start_delete(false, n),
            Action::DeletePermanent if self.at_root() => self.set_status(
                "D deletes files; in Locations dd removes an entry".into(),
                false,
            ),
            Action::DeletePermanent => self.start_delete(true, n),
            Action::NewFile => self.start_new(false),
            Action::NewDir => self.start_new(true),
            Action::BulkRename => self.start_bulk_rename(),
            Action::CommandLine => self.open_prompt(PromptKind::Command),
            Action::GotoPath => self.open_prompt_with(PromptKind::Command, "cd "),
            Action::SetMark(c) => self.set_mark(c),
            Action::JumpMark(c) => self.jump_mark(c),
            Action::PinItem => self.pin_item(),
            Action::EditPinTags => self.edit_pin_tags(),
            Action::Rename => self.start_rename(),
            Action::PinPicker => self.open_pin_picker(""),
            Action::TagPicker => self.open_tag_picker(),
            Action::MachinePicker => self.open_machine_picker(""),
            Action::ToggleMark => self.toggle_marks(n),
            Action::Visual => self.start_visual(),
            Action::Yank => self.yank(crate::jobs::Op::Copy, n),
            Action::Cut => self.yank(crate::jobs::Op::Move, n),
            Action::Paste => self.paste(false),
            Action::PasteOverwrite => self.paste(true),
            Action::Jobs => self.toggle_jobs_panel(),
            Action::RepeatPaste => self.repeat_paste(),
            Action::PinCwd => self.pin_cwd(),
            Action::TagRename(old, new) => self.tag_rename(&old, &new, None),
            Action::TagDelete(tag) => self.tag_delete(&tag, None),
            Action::ViewSave(name) => self.view_save(&name),
            Action::ViewDelete(name) => self.view_delete(&name),
            Action::Help => self.mode = Mode::Help,
            Action::Quit | Action::QuitNoWrite => self.quit = Some(Quit::Plain),
            Action::QuitWriteCwd => self.quit = Some(Quit::WithCwd),
            Action::Cd(path) => self.cd(&path),
            Action::NameSearch => self.open_prompt(PromptKind::NameSearch),
            Action::ContentSearch => self.open_prompt(PromptKind::ContentSearch),
            Action::NextPage => self.step_page(n, true),
            Action::FullPreview => self.toggle_full_preview(),
            Action::Connect(path) => self.connect_here(&path),
            Action::Machines => self.open_machines(),
            Action::Net(arg) => self.net_command(&arg),
            Action::Sync(name, skip) => self.sync_command(&name, skip),
            Action::SyncPicker => self.sync_command("", false),
            Action::Launcher => self.open_launcher(""),
            Action::FilesPicker => self.open_files_picker(false),
            Action::DirsPicker => self.open_dirs_picker(),
            Action::GrepPicker => self.open_grep_picker(),
            Action::CommandsPicker => self.open_commands_picker(false),
            Action::KeymapsPicker => self.open_commands_picker(true),
            Action::ImportWtm => self.import_wtm_command(),
            Action::PinUrl(url) => self.new_web_pin(&url),
            Action::OpenWith => self.open_menu(),
            Action::Reveal => self.open_targets_with("reveal"),
            Action::Properties => self.open_targets_with("properties"),
            Action::CopyPath => self.open_targets_with("copy-path"),
            Action::CopyUnc => self.open_targets_with("copy-unc"),
            Action::CopyName => self.copy_names(),
            Action::ExplorerHere => self.open_here("explorer"),
            Action::TerminalHere => self.open_here("terminal-here"),
            Action::Backup(name) => self.backup_command(&name),
            Action::PrevPage => self.step_page(n, false),
            Action::GotoPage => match count {
                Some(c) => self.goto_page(c),
                None => self.open_prompt_with(PromptKind::GotoPage, ""),
            },
            Action::NextMatch => self.step_match(n, true),
            Action::PrevMatch => self.step_match(n, false),
            Action::GotoResultFolder => self.goto_result_folder(),
            Action::CycleSort => self.cycle_sort(),
            Action::GotoLine(line) => self.set_cursor(line.saturating_sub(1)),
            Action::SetNumber(on) => self.number = on,
            Action::SetRelativeNumber(on) => self.relative_number = on,
        }
    }

    pub(super) fn cd(&mut self, text: &str) {
        let target =
            expand_home(text).and_then(|t| Loc::resolve(&self.cwd, &t).map_err(|e| e.to_string()));
        match target {
            Ok(loc) => self.navigate(loc, None),
            Err(e) => self.set_status(format!("cd: {e}"), true),
        }
    }

    pub(super) fn history(&mut self, n: usize, back: bool) {
        let mut target = None;
        for _ in 0..n.min(MAX_HISTORY_STEPS) {
            let (from, to) = if back {
                (&mut self.back, &mut self.forward)
            } else {
                (&mut self.forward, &mut self.back)
            };
            let Some(loc) = from.pop() else { break };
            to.push(target.replace(loc).unwrap_or_else(|| self.cwd.clone()));
        }
        match target {
            Some(loc) => self.set_cwd(loc, None),
            None => {
                let which = if back { "back" } else { "forward" };
                self.set_status(format!("nothing further {which} in history"), false);
            }
        }
    }

    // --------------------------------------------------------------------- search

    /// First row from `start` matching the current search, in `backward` order,
    /// wrapping around. Returns (row, wrapped).
    fn find(&self, start: usize, backward: bool, include_start: bool) -> Option<(usize, bool)> {
        let search = self.search.as_ref()?;
        let listing = self.view_listing.as_ref()?;
        let len = self.view.len();
        if len == 0 {
            return None;
        }
        let first = usize::from(!include_start);
        (first..first + len).find_map(|off| {
            let off = off % len;
            let row = if backward {
                (start + len - off) % len
            } else {
                (start + off) % len
            };
            let name = &listing.entries[self.view[row]].name;
            search.matches(name).then(|| {
                let wrapped = if backward { row > start } else { row < start };
                (row, wrapped)
            })
        })
    }

    /// `n` / `N`: `reverse` flips the search's own direction.
    fn search_again(&mut self, n: usize, reverse: bool) {
        let Some(search) = self.search.clone() else {
            self.set_status("no previous search".into(), true);
            return;
        };
        self.search_hl = true;
        let backward = search.backward != reverse;
        let mut wrapped_any = false;
        for _ in 0..n.min(self.view.len().max(1)) {
            match self.find(self.cursor, backward, false) {
                Some((row, wrapped)) => {
                    wrapped_any |= wrapped;
                    self.set_cursor(row);
                }
                None => {
                    self.set_status(format!("pattern not found: {}", search.query), true);
                    return;
                }
            }
        }
        if wrapped_any {
            let msg = if backward {
                "search hit TOP, continuing at BOTTOM"
            } else {
                "search hit BOTTOM, continuing at TOP"
            };
            self.set_status(msg.into(), false);
        }
    }

    pub fn is_search_match(&self, name: &str) -> bool {
        self.search_hl && self.search.as_ref().is_some_and(|s| s.matches(name))
    }

    // -------------------------------------------------------------------- prompts

    fn open_prompt(&mut self, kind: PromptKind) {
        let text = match kind {
            PromptKind::Filter => self.filter.clone(),
            // Prefilled inside results, so `s` refines the current search.
            PromptKind::NameSearch => self.current_search_query(false).unwrap_or_default(),
            PromptKind::ContentSearch => self.current_search_query(true).unwrap_or_default(),
            _ => String::new(),
        };
        self.open_prompt_with(kind, &text);
    }

    /// Opens a prompt with `text` typed in, the cursor at its end.
    pub(super) fn open_prompt_with(&mut self, kind: PromptKind, text: &str) {
        let mut p = Prompt {
            kind,
            editor: LineEditor::new(text),
            note: String::new(),
            origin: self.cursor,
            saved_search: self.search.clone(),
            completion: None,
            yes: true,
        };
        match kind {
            PromptKind::ConfirmUnpin | PromptKind::Confirm => p.note = self.pin_prompt_note(kind),
            PromptKind::Command
            | PromptKind::PinTags
            | PromptKind::MachineUser
            | PromptKind::Preset(_) => self.prompt_changed(&mut p),
            _ => {}
        }
        self.prompt = Some(p);
        self.mode = Mode::Prompt;
        // A destructive question starts on No: a stray Enter keeps everything.
        if self.dialog_view().is_some_and(|d| d.danger)
            && let Some(p) = self.prompt.as_mut()
        {
            p.yes = false;
        }
    }

    fn on_prompt_key(&mut self, ev: KeyEvent) {
        let Some(mut p) = self.prompt.take() else {
            self.mode = Mode::Normal;
            return;
        };
        // A yes/no popup: `y` yes, `n` / Esc / `q` no, `h` `l` Tab ← → pick a button and
        // Enter presses it. Other keys do nothing (a stray key isn't an answer).
        if matches!(p.kind, PromptKind::ConfirmUnpin | PromptKind::Confirm) {
            let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
            let answer = match ev.code {
                KeyCode::Char('y' | 'Y') if !ctrl => Some(true),
                KeyCode::Char('n' | 'N' | 'q') if !ctrl => Some(false),
                KeyCode::Esc => Some(false),
                KeyCode::Enter => Some(p.yes),
                KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right => {
                    p.yes = !p.yes;
                    None
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    p.yes = !p.yes;
                    None
                }
                _ => None,
            };
            match answer {
                Some(true) => {
                    self.mode = Mode::Normal;
                    self.prompt_submit(p);
                }
                Some(false) => {
                    self.mode = Mode::Normal;
                    self.prompt_cancel(p);
                }
                None => self.prompt = Some(p),
            }
            return;
        }
        match (p.kind, ev.code) {
            // `u` in a machine view: Tab cycles through the users found.
            (PromptKind::MachineUser, KeyCode::Tab | KeyCode::BackTab) => {
                let choices = self.machine_user_choices();
                if !choices.is_empty() {
                    let now = p.editor.text();
                    let at = choices.iter().position(|c| c.eq_ignore_ascii_case(&now));
                    let next = match (at, ev.code == KeyCode::Tab) {
                        (Some(i), true) => (i + 1) % choices.len(),
                        (Some(i), false) => (i + choices.len() - 1) % choices.len(),
                        (None, _) => 0,
                    };
                    p.editor = LineEditor::new(&choices[next]);
                }
                self.prompt = Some(p);
                return;
            }
            (PromptKind::Preset(f), KeyCode::Tab | KeyCode::BackTab) if f.is_path() => {
                self.complete(&mut p, ev.code == KeyCode::Tab);
                self.prompt = Some(p);
                return;
            }
            (PromptKind::PinTags, KeyCode::Tab) => {
                if let Some(text) = self.complete_tag(&p.editor.text()) {
                    p.editor = LineEditor::new(&text);
                    self.prompt_changed(&mut p);
                }
                self.prompt = Some(p);
                return;
            }
            (PromptKind::Command, KeyCode::Tab) => {
                self.complete(&mut p, true);
                self.prompt = Some(p);
                return;
            }
            (PromptKind::Command, KeyCode::BackTab) => {
                self.complete(&mut p, false);
                self.prompt = Some(p);
                return;
            }
            // The filter keeps the list live under it: arrows move the cursor.
            (PromptKind::Filter, KeyCode::Down) => {
                self.move_cursor(1);
                self.prompt = Some(p);
                return;
            }
            (PromptKind::Filter, KeyCode::Up) => {
                self.move_cursor(-1);
                self.prompt = Some(p);
                return;
            }
            _ => {}
        }
        let word = self.selected().map(|e| e.name.clone()).unwrap_or_default();
        match p.editor.handle(&ev, &word) {
            Edit::Moved | Edit::Ignored => {}
            Edit::Changed => self.prompt_changed(&mut p),
            Edit::Submit => {
                self.mode = Mode::Normal;
                self.prompt_submit(p);
                return;
            }
            Edit::Cancel => {
                self.mode = Mode::Normal;
                self.prompt_cancel(p);
                return;
            }
        }
        self.prompt = Some(p);
    }

    fn prompt_changed(&mut self, p: &mut Prompt) {
        let text = p.editor.text();
        match p.kind {
            PromptKind::Filter => {
                self.filter = text;
                self.rebuild_view();
            }
            PromptKind::Search { backward } => {
                // Incremental: jump to the first match from where the search started.
                self.search = Some(Search {
                    query: text.clone(),
                    backward,
                });
                self.search_hl = true;
                let hit = self.find(p.origin, backward, true);
                p.note = match (hit, text.is_empty()) {
                    (_, true) => String::new(),
                    (Some(_), _) => String::new(),
                    (None, _) => "no match".to_string(),
                };
                self.set_cursor(hit.map_or(p.origin, |(row, _)| row));
            }
            PromptKind::Command => {
                p.completion = None;
                p.note = command_note(&text);
            }
            // Validate as you type; the walk starts on Enter.
            PromptKind::NameSearch => {
                p.note = match crate::search::query::Query::parse(&text) {
                    Ok(_) => String::new(),
                    Err(e) => e.to_string(),
                };
            }
            // Also builds the regex, so `re:(` says "bad regex" before Enter.
            PromptKind::ContentSearch => {
                let parsed = crate::search::query::Query::parse(&text).map_err(|e| e.to_string());
                p.note = match parsed.and_then(|q| crate::search::SearchMatcher::new(&q, true)) {
                    Ok(_) => String::new(),
                    Err(_) if text.trim().is_empty() => String::new(),
                    Err(e) => e,
                };
            }
            // Completion candidates for the word being typed, most used first.
            PromptKind::PinTags => {
                let shown: Vec<String> = self
                    .tag_candidates(&text)
                    .into_iter()
                    .take(6)
                    .map(|(t, n)| format!("#{t} {n}"))
                    .collect();
                p.note = match (shown.is_empty(), crate::pins::parse_tags(&text)) {
                    (_, Err(e)) => e,
                    (true, Ok(_)) => "space-separated, Enter for none".to_string(),
                    (false, Ok(_)) => format!("Tab: {}", shown.join(" · ")),
                };
            }
            PromptKind::PinName
            | PromptKind::PinKey
            | PromptKind::ConfirmUnpin
            | PromptKind::Confirm
            | PromptKind::ViewName
            | PromptKind::ViewKey
            | PromptKind::TagRename
            | PromptKind::Rename
            | PromptKind::GotoPage => p.note.clear(),
            PromptKind::NewFile => p.note = r"end in \ for a folder; sub\name makes sub too".into(),
            PromptKind::NewDir => p.note = r"sub\name makes both".into(),
            PromptKind::Preset(f) => p.note = self.preset_note(f, &text),
            PromptKind::MachineUser => {
                let choices = self.machine_user_choices();
                p.note = if choices.is_empty() {
                    "a user name (its profile folder)".to_string()
                } else {
                    format!("Tab: {}", choices.join(" · "))
                };
            }
        }
    }

    fn prompt_submit(&mut self, p: Prompt) {
        let text = p.editor.text();
        match p.kind {
            PromptKind::Command => {
                if text.trim().is_empty() {
                    return;
                }
                match Action::parse(&text) {
                    Ok(action) => self.run(action, None),
                    Err(e) => self.set_status(format!(":{}: {e}", text.trim()), true),
                }
            }
            PromptKind::Search { .. } if text.is_empty() => {
                // `/` + Enter repeats the last search, as in vim.
                self.search = p.saved_search;
                self.search_again(1, false);
            }
            PromptKind::Search { .. } => {
                if self.find(p.origin, false, true).is_none() {
                    self.set_status(format!("pattern not found: {text}"), true);
                }
            }
            PromptKind::Filter => {}
            PromptKind::NameSearch if text.trim().is_empty() => self.set_status(
                "search for what? (s then text, *.pdf, re:…, ext:pdf)".into(),
                false,
            ),
            PromptKind::NameSearch => self.start_search(&text, false),
            PromptKind::ContentSearch if text.trim().is_empty() => self.set_status(
                "search contents for what? (S then text, re:…, ext:pmlmac)".into(),
                false,
            ),
            PromptKind::ContentSearch => self.start_search(&text, true),
            PromptKind::PinName
            | PromptKind::PinTags
            | PromptKind::PinKey
            | PromptKind::ConfirmUnpin
            | PromptKind::Confirm
            | PromptKind::ViewName
            | PromptKind::ViewKey
            | PromptKind::TagRename
            | PromptKind::Preset(_) => {
                if let Err(note) = self.pin_submit(p.kind, &text) {
                    // Stay in the prompt and say what's wrong.
                    let mut p = p;
                    p.note = note;
                    self.prompt = Some(p);
                    self.mode = Mode::Prompt;
                }
            }
            PromptKind::MachineUser => self.machine_user_picked(&text),
            PromptKind::Rename | PromptKind::NewFile | PromptKind::NewDir => {
                if let Err(note) = self.fileop_submit(p.kind, &text) {
                    let mut p = p;
                    p.note = note;
                    self.prompt = Some(p);
                    self.mode = Mode::Prompt;
                }
            }
            PromptKind::GotoPage => match text.trim().parse::<usize>() {
                Ok(n) if n > 0 => self.goto_page(n),
                _ if text.trim().is_empty() => {}
                _ => self.set_status(format!("not a page number: {}", text.trim()), true),
            },
        }
    }

    fn prompt_cancel(&mut self, p: Prompt) {
        match p.kind {
            PromptKind::Filter => {
                self.filter.clear();
                self.rebuild_view();
            }
            PromptKind::Search { .. } => {
                self.search = p.saved_search;
                self.set_cursor(p.origin);
            }
            PromptKind::Command | PromptKind::NameSearch | PromptKind::ContentSearch => {}
            PromptKind::PinName
            | PromptKind::PinTags
            | PromptKind::PinKey
            | PromptKind::ConfirmUnpin
            | PromptKind::Confirm
            | PromptKind::ViewName
            | PromptKind::ViewKey
            | PromptKind::TagRename
            | PromptKind::Preset(_) => self.pin_cancel(p.kind),
            PromptKind::GotoPage => {}
            PromptKind::MachineUser => self.machine_user_host = None,
            PromptKind::Rename | PromptKind::NewFile | PromptKind::NewDir => self.fileop_cancel(),
        }
    }

    // ----------------------------------------------------------------- completion

    fn complete(&mut self, p: &mut Prompt, forward: bool) {
        let line = p.editor.text();
        if let Some(c) = p.completion.as_mut()
            && c.applied == line
            && !c.candidates.is_empty()
        {
            let len = c.candidates.len();
            c.idx = if forward {
                (c.idx + 1) % len
            } else {
                (c.idx + len - 1) % len
            };
            apply_completion(p);
            return;
        }
        // A preset's folder field is all path; the command line has `cd ` first.
        let arg = if matches!(p.kind, PromptKind::Preset(_)) {
            line.as_str()
        } else {
            let Some((cmd, arg)) = line.split_once(' ') else {
                p.note = "Tab completes the path after :cd and :e".to_string();
                return;
            };
            if !matches!(cmd, "cd" | "e" | "edit") {
                p.note = "Tab completes the path after :cd and :e".to_string();
                return;
            }
            arg
        };
        let arg = arg.trim_start();
        let head = line[..line.len() - arg.len()].to_string();
        let (dir_text, prefix) = split_for_completion(arg);
        let dir = if dir_text.is_empty() {
            Ok(self.cwd.clone())
        } else {
            expand_home(dir_text)
                .and_then(|t| Loc::resolve(&self.cwd, &t).map_err(|e| e.to_string()))
        };
        let dir = match dir {
            Ok(d) => d,
            Err(e) => {
                p.note = e;
                return;
            }
        };
        p.completion = Some(Completion {
            head,
            dir_text: dir_text.to_string(),
            dir: dir.clone(),
            prefix: prefix.to_string(),
            candidates: Vec::new(),
            loaded: false,
            idx: 0,
            applied: line,
        });
        if !self.fill_completion(p) {
            p.note = format!("listing {}…", dir.display());
            self.request(dir, false);
        }
    }

    /// Fills candidates from the cached listing. False if it isn't cached yet.
    fn fill_completion(&self, p: &mut Prompt) -> bool {
        let Some(c) = p.completion.as_mut() else {
            return true;
        };
        let state = self.dirs.get(&c.dir);
        let Some(listing) = state.and_then(|s| s.listing.clone()) else {
            if let Some(err) = state.and_then(|s| s.error.as_ref()) {
                c.loaded = true;
                p.note = format!("can't complete: {err}");
                return true;
            }
            return false;
        };
        let pre = c.prefix.to_lowercase();
        c.candidates = listing
            .entries
            .iter()
            .filter(|e| e.is_container() && (self.show_hidden || !e.is_hidden()))
            .filter(|e| e.name.to_lowercase().starts_with(&pre))
            .map(|e| e.name.clone())
            .collect();
        c.loaded = true;
        c.idx = 0;
        if c.candidates.is_empty() {
            p.note = "no match".to_string();
        } else {
            apply_completion(p);
        }
        true
    }

    /// From `on_listed`: a directory a completion was waiting for has arrived.
    pub(super) fn completion_listed(&mut self, loc: &Loc) {
        let Some(mut p) = self.prompt.take() else {
            return;
        };
        if p.completion
            .as_ref()
            .is_some_and(|c| !c.loaded && c.dir == *loc)
        {
            self.fill_completion(&mut p);
            self.dirty = true;
        }
        self.prompt = Some(p);
    }

    /// The directory an open completion is waiting on, so it isn't cancelled.
    pub(super) fn completion_dir(&self) -> Option<Loc> {
        let c = self.prompt.as_ref()?.completion.as_ref()?;
        (!c.loaded).then(|| c.dir.clone())
    }
}

const MAX_HISTORY_STEPS: usize = super::MAX_HISTORY;

fn apply_completion(p: &mut Prompt) {
    let Some(c) = p.completion.as_mut() else {
        return;
    };
    let Some(name) = c.candidates.get(c.idx) else {
        return;
    };
    let mut dir = c.dir_text.clone();
    if !dir.is_empty() && !dir.ends_with(['\\', '/']) {
        dir.push('\\');
    }
    let text = format!("{}{dir}{name}\\", c.head);
    p.editor.set_text(&text);
    p.note = format!(
        "{}/{} (Tab next, S-Tab previous)",
        c.idx + 1,
        c.candidates.len()
    );
    c.applied = text;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_expansion() {
        let Ok(h) = home() else { return };
        assert_eq!(expand_home("~"), Ok(h.clone()));
        assert_eq!(expand_home("~\\Downloads"), Ok(format!("{h}\\Downloads")));
        assert_eq!(expand_home(r"C:\~x"), Ok(r"C:\~x".to_string()));
        assert_eq!(expand_home("~user"), Ok("~user".to_string()));
    }
}
