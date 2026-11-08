//! Tag management and saved views (CLAUDE.md section 3):
//!
//! - `:tags` / `<leader>t`: the tag picker (counts; Tab chooses, Enter filters pins)
//! - `:tag rename <old> <new>`, `:tag delete <tag>`: across all pins, after a y/n that
//!   says how many pins change (`^e` / `^x` in the tag picker do the same)
//! - `:view save <name>`: the last pin-picker query as a `[[view]]`, then its `'` key;
//!   `:view delete <name>`. In the pin picker a view is a `[view]` row (Enter applies
//!   it, `^e` renames / rekeys, `^x` deletes).

use super::App;
use super::keys::PromptKind;
use super::picker::Reopen;
use crate::pins::{self, View};

/// A change that asks first.
#[derive(Debug, Clone)]
pub enum Confirm {
    DeleteView {
        idx: usize,
        reopen: Option<Reopen>,
    },
    RenameTag {
        old: String,
        new: String,
        reopen: Option<Reopen>,
    },
    DeleteTag {
        tag: String,
        reopen: Option<Reopen>,
    },
    /// A paste that replaces files (`P`) or touches an AVEVA project.
    Paste {
        req: Box<crate::jobs::Request>,
        plan: Box<crate::jobs::Plan>,
        why: String,
    },
    /// Importing wtm's launcher entries (`offer`: asked at first run, not `:import-wtm`).
    ImportWtm {
        pins: Vec<crate::pins::Pin>,
        usage: Box<crate::frecency::Usage>,
        offer: bool,
        why: String,
    },
    /// Running a backup preset (`direct`: zip the source without staging).
    Backup {
        preset: Box<crate::jobs::zip::BackupPreset>,
        direct: bool,
        why: String,
    },
    /// A zip or an extract on an admin share.
    Archive {
        job: Box<crate::jobs::archive::ArchiveJob>,
        why: String,
    },
    /// A delete, or a change on an admin share / a bulk rename.
    FileOp {
        op: crate::fs::ops::FileOp,
        why: String,
    },
}

impl Confirm {
    fn reopen(self) -> Option<Reopen> {
        match self {
            Confirm::DeleteView { reopen, .. }
            | Confirm::RenameTag { reopen, .. }
            | Confirm::DeleteTag { reopen, .. } => reopen,
            Confirm::Paste { .. }
            | Confirm::FileOp { .. }
            | Confirm::Backup { .. }
            | Confirm::Archive { .. }
            | Confirm::ImportWtm { .. } => None,
        }
    }
}

/// A view being renamed / rekeyed, or just saved (then only the key step runs).
#[derive(Debug, Clone)]
pub struct ViewDraft {
    idx: usize,
    name: String,
    reopen: Option<Reopen>,
}

impl ViewDraft {
    pub(super) fn idx(&self) -> usize {
        self.idx
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn reopen(self) -> Option<Reopen> {
        self.reopen
    }
}

fn pins_word(n: usize) -> String {
    if n == 1 {
        "1 pin".to_string()
    } else {
        format!("{n} pins")
    }
}

impl App {
    // ---------------------------------------------------------------- commands

    /// `:tag rename <old> <new>`.
    pub(super) fn tag_rename(&mut self, old: &str, new: &str, reopen: Option<Reopen>) {
        let (old, new) = match (pins::normalize_tag(old), pins::normalize_tag(new)) {
            (Ok(o), Ok(n)) => (o, n),
            (Err(e), _) | (_, Err(e)) => {
                self.set_status(e, true);
                self.reopen_picker(reopen);
                return;
            }
        };
        if old == new || !self.pins_ready() {
            self.reopen_picker(reopen);
            return;
        }
        if self.pins.tag_count(&old) == 0 {
            self.set_status(format!("no pin is tagged #{old}"), true);
            self.reopen_picker(reopen);
            return;
        }
        self.ask(Confirm::RenameTag { old, new, reopen });
    }

    /// `:tag delete <tag>`.
    pub(super) fn tag_delete(&mut self, tag: &str, reopen: Option<Reopen>) {
        let tag = match pins::normalize_tag(tag) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(e, true);
                return;
            }
        };
        if !self.pins_ready() {
            return;
        }
        if self.pins.tag_count(&tag) == 0 {
            self.set_status(format!("no pin is tagged #{tag}"), true);
            self.reopen_picker(reopen);
            return;
        }
        self.ask(Confirm::DeleteTag { tag, reopen });
    }

    /// `:view save <name>`: the last pin-picker query, then asks for its key.
    pub(super) fn view_save(&mut self, name: &str) {
        let name = name.trim();
        let query = self.last_picker_query.trim().to_string();
        if name.is_empty() {
            self.set_status("view save what? (:view save <name>)".into(), true);
            return;
        }
        if query.is_empty() {
            self.set_status(
                "no query to save: type one in the pin picker (<leader>p) first".into(),
                true,
            );
            return;
        }
        if !self.pins_ready() {
            return;
        }
        let idx = self.pins.save_view(View::new(name, &query));
        self.save_pins();
        self.set_status(format!("view {name}: {query}"), false);
        let key = self.pins.views[idx].key.clone().unwrap_or_default();
        self.view_draft = Some(ViewDraft {
            idx,
            name: name.to_string(),
            reopen: None,
        });
        self.open_prompt_with(PromptKind::ViewKey, &key);
    }

    /// `:view delete <name>`.
    pub(super) fn view_delete(&mut self, name: &str) {
        match self
            .pins
            .views
            .iter()
            .position(|v| v.name.eq_ignore_ascii_case(name.trim()))
        {
            Some(idx) => self.confirm_view_delete(idx, None),
            None => self.set_status(format!("no view named {}", name.trim()), true),
        }
    }

    pub(super) fn confirm_view_delete(&mut self, idx: usize, reopen: Option<Reopen>) {
        if self.pins_ready() {
            self.ask(Confirm::DeleteView { idx, reopen });
        }
    }

    /// `^e` on a view in the pin picker: name, then key.
    pub(super) fn edit_view(&mut self, idx: usize, reopen: Option<Reopen>) {
        if !self.pins_ready() {
            return;
        }
        let Some(name) = self.pins.views.get(idx).map(|v| v.name.clone()) else {
            return;
        };
        self.view_draft = Some(ViewDraft {
            idx,
            name: name.clone(),
            reopen,
        });
        self.open_prompt_with(PromptKind::ViewName, &name);
    }

    /// `^e` on a tag in the tag picker: its new name, then a y/n.
    pub(super) fn start_tag_rename(&mut self, tag: &str) {
        self.tag_draft = Some(tag.to_string());
        self.open_prompt_with(PromptKind::TagRename, tag);
    }

    // ------------------------------------------------------------------ prompts

    fn ask(&mut self, c: Confirm) {
        self.confirm = Some(c);
        self.open_prompt_with(PromptKind::Confirm, "");
    }

    /// The pending question with its key hint (the status line, tests).
    pub(super) fn confirm_note(&self) -> String {
        format!("{} y = yes, n = no", self.confirm_question())
    }

    /// The pending question alone (the popup's text).
    pub fn confirm_question(&self) -> String {
        match &self.confirm {
            Some(Confirm::DeleteView { idx, .. }) => {
                let name = self.pins.views.get(*idx).map_or("?", |v| v.name.as_str());
                format!("delete view {name}?")
            }
            Some(Confirm::RenameTag { old, new, .. }) => {
                let n = self.pins.tag_count(old);
                let merge = if self.pins.tag_count(new) > 0 {
                    format!(" (merges into the existing #{new})")
                } else {
                    String::new()
                };
                format!("rename #{old} → #{new} on {}{merge}?", pins_word(n))
            }
            Some(Confirm::DeleteTag { tag, .. }) => {
                let n = self.pins.tag_count(tag);
                format!("remove #{tag} from {}?", pins_word(n))
            }
            Some(Confirm::FileOp { why, .. }) => why.clone(),
            Some(
                Confirm::Paste { why, .. }
                | Confirm::Backup { why, .. }
                | Confirm::Archive { why, .. }
                | Confirm::ImportWtm { why, .. },
            ) => format!("{why} Go ahead?"),
            None => String::new(),
        }
    }

    /// The popup for the pending question (`Confirm`) or unpin: its title, text, the
    /// items it touches, and whether it destroys something (red, No pre-selected).
    pub fn dialog_view(&self) -> Option<super::DialogView> {
        let p = self.prompt.as_ref()?;
        if p.kind == PromptKind::ExtractExists {
            return self.extract_dialog(p.yes);
        }
        let mut items: Vec<String> = Vec::new();
        let name = |s: &str| {
            s.trim_end_matches('\\')
                .rsplit('\\')
                .next()
                .unwrap_or(s)
                .to_string()
        };
        let (title, danger) = match p.kind {
            PromptKind::ConfirmUnpin => {
                let d = self.pin_draft.as_ref()?;
                items.push(format!("{}  {}", d.pin.name, d.pin.detail()));
                ("Unpin".to_string(), false)
            }
            PromptKind::Confirm => match self.confirm.as_ref()? {
                Confirm::FileOp { op, .. } => {
                    use crate::fs::ops::FileOp;
                    match op {
                        FileOp::Trash(ps) | FileOp::Delete(ps) => {
                            items.extend(ps.iter().map(|s| name(s)));
                        }
                        FileOp::Rename { from, to } => {
                            items.push(format!("{} → {}", name(from), name(to)));
                        }
                        FileOp::BulkRename(pairs) => items.extend(
                            pairs
                                .iter()
                                .map(|(a, b)| format!("{} → {}", name(a), name(b))),
                        ),
                        FileOp::UndoRename(u) => items.extend(u.iter().flat_map(|u| {
                            u.pairs
                                .iter()
                                .map(|(a, b)| format!("{} → {}", name(a), name(b)))
                        })),
                        FileOp::Mkdir(s) | FileOp::NewFile(s) => items.push(name(s)),
                    }
                    match op {
                        FileOp::Trash(_) => ("Move to the Recycle Bin".to_string(), false),
                        FileOp::Delete(_) => ("Delete permanently".to_string(), true),
                        FileOp::Rename { .. } | FileOp::BulkRename(_) => ("Rename".into(), false),
                        FileOp::UndoRename(_) => ("Undo rename".into(), false),
                        FileOp::Mkdir(_) | FileOp::NewFile(_) => ("Create".to_string(), false),
                    }
                }
                Confirm::Paste { req, .. } => {
                    items.extend(req.items.iter().map(|i| i.path.name()));
                    let replace = req.overwrite == crate::jobs::robocopy::Overwrite::Always;
                    (
                        if replace { "Paste, replacing" } else { "Paste" }.to_string(),
                        replace,
                    )
                }
                Confirm::Backup { .. } => ("Backup".to_string(), false),
                Confirm::Archive { job, .. } => (
                    match **job {
                        crate::jobs::archive::ArchiveJob::Compress { .. } => "Zip",
                        crate::jobs::archive::ArchiveJob::Extract { .. } => "Extract",
                    }
                    .to_string(),
                    false,
                ),
                Confirm::ImportWtm { pins, .. } => {
                    items.extend(pins.iter().map(|p| p.name.clone()));
                    ("Import from wtm".to_string(), false)
                }
                Confirm::DeleteView { .. } => ("Delete view".to_string(), true),
                Confirm::RenameTag { .. } => ("Rename tag".to_string(), false),
                Confirm::DeleteTag { .. } => ("Remove tag".to_string(), true),
            },
            _ => return None,
        };
        let message = match p.kind {
            PromptKind::ConfirmUnpin => "unpin it? (the folder itself is not touched)".to_string(),
            _ => self.confirm_question(),
        };
        Some(super::DialogView {
            title,
            message,
            items,
            danger,
            yes: p.yes,
            buttons: ["Yes", "No"],
            keys: "y yes · n/Esc no · h/l Tab choose · Enter",
        })
    }

    pub(super) fn store_submit(&mut self, kind: PromptKind, text: &str) -> Result<(), String> {
        match kind {
            PromptKind::Confirm => {
                if let Some(c) = self.confirm.take() {
                    self.apply_confirm(c);
                }
            }
            PromptKind::ViewName => {
                let name = text.trim();
                if name.is_empty() {
                    return Err("a view needs a name".to_string());
                }
                let taken = self.pins.views.iter().enumerate().any(|(i, v)| {
                    v.name.eq_ignore_ascii_case(name)
                        && self.view_draft.as_ref().is_some_and(|d| d.idx != i)
                });
                if taken {
                    return Err(format!("there's already a view named {name}"));
                }
                let key = match self.view_draft.as_mut() {
                    Some(d) => {
                        d.name = name.to_string();
                        self.pins
                            .views
                            .get(d.idx)
                            .and_then(|v| v.key.clone())
                            .unwrap_or_default()
                    }
                    None => return Ok(()),
                };
                self.open_prompt_with(PromptKind::ViewKey, &key);
            }
            PromptKind::ViewKey => {
                let key = pins::parse_key(text)?;
                let Some(d) = self.view_draft.take() else {
                    return Ok(());
                };
                self.give_view_key(d, key);
            }
            PromptKind::TagRename => {
                let Some(old) = self.tag_draft.take() else {
                    return Ok(());
                };
                if let Err(e) = pins::normalize_tag(text) {
                    self.tag_draft = Some(old);
                    return Err(e);
                }
                self.tag_rename(&old, text, Some(Reopen::Tags));
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn store_cancel(&mut self, kind: PromptKind) {
        let reopen = match kind {
            PromptKind::Confirm => match self.confirm.take() {
                // Declining the first-run offer writes pins.toml, so it isn'"'"'t asked again.
                Some(Confirm::ImportWtm { offer: true, .. }) => {
                    self.save_pins();
                    return self.set_status(
                        "not imported from wtm; :import-wtm does it any time".into(),
                        false,
                    );
                }
                c => c.and_then(Confirm::reopen),
            },
            PromptKind::ViewName | PromptKind::ViewKey => {
                self.view_draft.take().and_then(|d| d.reopen)
            }
            PromptKind::TagRename => self.tag_draft.take().map(|_| Reopen::Tags),
            _ => None,
        };
        self.set_status("unchanged".into(), false);
        self.reopen_picker(reopen);
        // The bulk rename panel asked: back to it.
        self.rename_answered(false);
    }

    fn apply_confirm(&mut self, c: Confirm) {
        if let Confirm::ImportWtm { pins, usage, .. } = c {
            return self.import_wtm(pins, *usage);
        }
        if let Confirm::Archive { job, .. } = c {
            return self.start_archive(*job);
        }
        if let Confirm::Backup { preset, direct, .. } = c {
            return self.run_backup(*preset, direct);
        }
        if let Confirm::Paste { req, plan, .. } = c {
            return self.start_job(*req, *plan);
        }
        if let Confirm::FileOp { op, .. } = c {
            self.rename_answered(true);
            return self.run_op(op);
        }
        let msg = match &c {
            Confirm::DeleteView { idx, .. } if *idx < self.pins.views.len() => {
                let v = self.pins.views.remove(*idx);
                format!("deleted view {}", v.name)
            }
            Confirm::DeleteView { .. } => String::new(),
            Confirm::RenameTag { old, new, .. } => {
                let n = self.pins.rename_tag(old, new);
                format!("#{old} → #{new} on {}", pins_word(n))
            }
            Confirm::DeleteTag { tag, .. } => {
                let n = self.pins.delete_tag(tag);
                format!("removed #{tag} from {}", pins_word(n))
            }
            Confirm::Paste { .. }
            | Confirm::FileOp { .. }
            | Confirm::Backup { .. }
            | Confirm::Archive { .. }
            | Confirm::ImportWtm { .. } => String::new(),
        };
        self.save_pins();
        self.set_status(msg, false);
        self.reopen_picker(c.reopen());
    }
}

impl App {
    /// The view's key step, once its key clashes with nothing: rename, rekey, save.
    pub(super) fn finish_view_key(&mut self, d: ViewDraft, key: Option<String>) {
        let Some(view) = self.pins.views.get_mut(d.idx) else {
            return;
        };
        view.name = d.name.clone();
        let moved = self.pins.set_view_key(d.idx, key.as_deref());
        self.save_pins();
        let msg = match (key, moved) {
            (Some(k), Some(old)) => format!("'{k} → view {} (was {old})", d.name),
            (Some(k), None) => format!("'{k} → view {}", d.name),
            (None, _) => format!("saved view {}", d.name),
        };
        self.set_status(msg, false);
        self.reopen_picker(d.reopen);
    }
}
