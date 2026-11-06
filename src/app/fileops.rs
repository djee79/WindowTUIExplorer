//! File operations (CLAUDE.md section 5), run by the worker:
//!
//! - `r` / `cw`: rename the item under the cursor; the prompt's cursor starts before the
//!   extension. (In Locations `r` still renames a pin.)
//! - `a`: new file (`sub\x.txt` makes `sub` too; a trailing `\` makes a folder), `A`: new
//!   folder.
//! - `dd`: to the Recycle Bin (asks). On a share or a mapped drive there is no Recycle
//!   Bin, and the question says the delete is permanent. `D`: permanent delete; type `yes`.
//!   Both take the selection, the visual range, or `n` items (`3dd`).
//! - `:rename`: the selection's names (or the whole folder's) in `$EDITOR`, one per line;
//!   the changed lines are renamed after a y/n that lists them.
//! - On an admin share (`\\HOST\C$`), every change asks first, naming the machine and its
//!   user, and is written to `%LOCALAPPDATA%\sentier\audit.log`.

use super::keys::PromptKind;
use super::tags::Confirm;
use super::{App, Mode};
use crate::fs::EntryKind;
use crate::fs::ops::{self, Done, FileOp};
use crate::fs::path::{FsPath, Loc};

/// What the open file-operation prompt is about.
#[derive(Debug, Clone)]
pub enum OpDraft {
    Rename { from: FsPath },
}

/// A program to run in this terminal with the TUI suspended.
#[derive(Debug, Clone)]
pub struct Inline {
    pub program: String,
    pub args: Vec<String>,
    /// Written before the program runs, read back after (the bulk-rename list).
    pub file: Option<(std::path::PathBuf, String)>,
    pub then: InlineThen,
}

#[derive(Debug, Clone)]
pub enum InlineThen {
    /// `:rename` in `dir`, of these names.
    BulkRename { dir: FsPath, names: Vec<String> },
    /// An inline opener (Neovim…): list the folder again after, it may have changed.
    Refresh { what: String },
}

/// How the inline program went: its exit, and the file's text afterwards.
#[derive(Debug, Clone)]
pub struct InlineResult {
    pub status: Result<i32, String>,
    pub text: Option<String>,
}

impl App {
    /// The machine and its user when `loc` is on an admin share.
    fn admin_target(&self, loc: &Loc) -> Option<(String, Option<String>)> {
        let host = loc.admin_host()?.to_string();
        let user = self.machine_user(&host);
        Some((host, user))
    }

    /// `On JDOE-LT042 (jdoe's machine): `
    pub(super) fn admin_prefix(&self) -> Option<String> {
        let (host, user) = self.admin_target(&self.cwd)?;
        Some(match user {
            Some(u) => format!("On {host} ({u}'s machine): "),
            None => format!("On {host}: "),
        })
    }

    // ---------------------------------------------------------------- rename

    /// `r` / `cw`.
    pub(super) fn start_rename(&mut self) {
        if self.at_root() {
            return self.rename_pin();
        }
        let Some(e) = self.selected().cloned() else {
            return;
        };
        if !matches!(e.kind, EntryKind::File | EntryKind::Dir) {
            return self.set_status("only files and folders can be renamed".into(), false);
        }
        let Ok(Loc::Fs(from)) = self.cwd.join(&e.name) else {
            return self.set_status("nothing to rename here".into(), false);
        };
        let name = from.name();
        let at = ops::rename_cursor(&name, e.kind == EntryKind::Dir);
        self.op_draft = Some(OpDraft::Rename { from });
        self.open_prompt_with(PromptKind::Rename, &name);
        if let Some(p) = self.prompt.as_mut() {
            p.editor.set_cursor(at);
        }
    }

    /// `a` / `A`.
    pub(super) fn start_new(&mut self, dir: bool) {
        if self.cwd.as_fs().is_none() {
            return self.set_status("new files go in a folder".into(), false);
        }
        self.open_prompt_with(
            if dir {
                PromptKind::NewDir
            } else {
                PromptKind::NewFile
            },
            "",
        );
    }

    // ---------------------------------------------------------------- delete

    /// `dd` (to the Recycle Bin where there is one) or `D` (permanent).
    pub(super) fn start_delete(&mut self, permanent: bool, count: usize) {
        let Some(listing) = self.view_listing.clone() else {
            return;
        };
        let mut paths: Vec<FsPath> = Vec::new();
        for i in self.operand(count) {
            let e = &listing.entries[i];
            if !matches!(e.kind, EntryKind::File | EntryKind::Dir) {
                continue;
            }
            if let Ok(Loc::Fs(p)) = self.cwd.join(&e.name) {
                paths.push(p);
            }
        }
        self.visual = None;
        if paths.is_empty() {
            return self.set_status("nothing to delete here".into(), false);
        }
        let mapped = self.mapped_servers();
        let remote = paths
            .iter()
            .any(|p| p.is_network() || p.drive().is_some_and(|l| mapped.contains_key(&l)));
        let shown: Vec<String> = paths.iter().map(FsPath::display).collect();
        let what = match paths.as_slice() {
            [one] => one.name(),
            many => format!("{} items", many.len()),
        };
        let admin = self.admin_prefix().unwrap_or_default();
        if permanent {
            // A red popup with No pre-selected: `y` deletes, Enter alone keeps them.
            let why = format!("{admin}delete {what} PERMANENTLY? There is no undo.");
            self.confirm = Some(Confirm::FileOp {
                op: FileOp::Delete(shown),
                why,
            });
            self.open_prompt_with(PromptKind::Confirm, "");
            return;
        }
        let (op, why) = if remote {
            (
                FileOp::Delete(shown),
                format!("{admin}delete {what}? Permanent: no Recycle Bin over the network."),
            )
        } else {
            (
                FileOp::Trash(shown),
                format!("{admin}move {what} to the Recycle Bin?"),
            )
        };
        self.confirm = Some(Confirm::FileOp { op, why });
        self.open_prompt_with(PromptKind::Confirm, "");
    }

    // ------------------------------------------------------------- prompts

    /// Enter on a rename / new / `yes` prompt. Err keeps the prompt open with a note.
    pub(super) fn fileop_submit(&mut self, kind: PromptKind, text: &str) -> Result<(), String> {
        match kind {
            PromptKind::Rename => {
                let name = text.trim();
                ops::check_name(name, false)?;
                let Some(OpDraft::Rename { from }) = self.op_draft.take() else {
                    return Ok(());
                };
                if name == from.name() {
                    self.set_status("unchanged".into(), false);
                    return Ok(());
                }
                let Some(to) = Loc::Fs(from.clone())
                    .parent()
                    .and_then(|p| p.join(name).ok())
                else {
                    return Err(format!("can't rename to {name}"));
                };
                let op = FileOp::Rename {
                    from: from.display(),
                    to: to.display(),
                };
                self.confirm_admin_or_run(op);
            }
            PromptKind::NewFile | PromptKind::NewDir => {
                let dir = kind == PromptKind::NewDir || text.trim_end().ends_with('\\');
                let name = text.trim().trim_end_matches('\\');
                ops::check_name(name, true)?;
                let Some(base) = self.cwd.as_fs() else {
                    return Ok(());
                };
                let full = format!("{}\\{name}", base.display().trim_end_matches('\\'));
                let op = if dir {
                    FileOp::Mkdir(full)
                } else {
                    FileOp::NewFile(full)
                };
                self.confirm_admin_or_run(op);
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn fileop_cancel(&mut self) {
        self.op_draft = None;
        self.set_status("unchanged".into(), false);
    }

    /// On an admin share, a y/n naming the machine first; elsewhere straight on.
    fn confirm_admin_or_run(&mut self, op: FileOp) {
        match self.admin_prefix() {
            Some(prefix) => {
                let why = format!("{prefix}{}?", op.describe());
                self.confirm = Some(Confirm::FileOp { op, why });
                self.open_prompt_with(PromptKind::Confirm, "");
            }
            None => self.run_op(op),
        }
    }

    /// Hands the change to the worker (with the machine, for the audit log).
    pub(super) fn run_op(&mut self, op: FileOp) {
        let audit = if self.config.machines.audit.unwrap_or(true) {
            self.admin_target(&self.cwd)
        } else {
            None
        };
        self.set_status(format!("{}…", op.describe()), false);
        self.ops_running += 1;
        self.marked.clear();
        self.worker.file_op(op, audit);
    }

    pub(super) fn on_file_op_done(&mut self, op: FileOp, result: Result<Done, String>) {
        self.ops_running = self.ops_running.saturating_sub(1);
        match result {
            Ok(done) => {
                // `a sub\x.txt`: the cursor goes on `sub`, what appeared here.
                let here = match &op {
                    FileOp::Mkdir(p) | FileOp::NewFile(p) => self.first_under_cwd(p),
                    _ => None,
                };
                if let Some(name) = here.or(done.select) {
                    self.select_next = Some(name);
                }
                self.set_status(done.message, false);
            }
            Err(e) => self.set_status(format!("{}: {e}", op.describe()), true),
        }
        self.request(self.cwd.clone(), true);
        if let Some(p) = self.preview_loc() {
            self.request(p, true);
        }
    }

    /// The first component of `path` below the current folder.
    fn first_under_cwd(&self, path: &str) -> Option<String> {
        let base = self.cwd.as_fs()?.display();
        let base = base.trim_end_matches('\\');
        let rest = path.get(base.len()..)?;
        if !path[..base.len()].eq_ignore_ascii_case(base) {
            return None;
        }
        rest.trim_start_matches('\\')
            .split('\\')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    // ------------------------------------------------------------ bulk rename

    /// `:rename`: the selection (or every item here) in `$EDITOR`.
    pub(super) fn start_bulk_rename(&mut self) {
        let Some(dir) = self.cwd.as_fs().cloned() else {
            return self.set_status(
                ":rename works in a folder (not in search results yet)".into(),
                false,
            );
        };
        let Some(listing) = self.view_listing.clone() else {
            return;
        };
        let idx = if self.visual.is_some() || !self.marked.is_empty() {
            self.operand(1)
        } else {
            self.view.clone()
        };
        let names: Vec<String> = idx
            .iter()
            .map(|&i| &listing.entries[i])
            .filter(|e| matches!(e.kind, EntryKind::File | EntryKind::Dir))
            .map(|e| e.name.clone())
            .collect();
        self.visual = None;
        if names.is_empty() {
            return self.set_status("nothing to rename here".into(), false);
        }
        let editor = self.config.editor();
        let mut words = editor.split_whitespace().map(str::to_string);
        let program = words.next().unwrap_or_else(|| "nvim".into());
        let file = std::env::temp_dir().join(format!("sentier-rename-{}.txt", std::process::id()));
        let mut args: Vec<String> = words.collect();
        args.push(file.display().to_string());
        let mut text = names.join("\r\n");
        text.push_str("\r\n");
        self.inline = Some(Inline {
            program,
            args,
            file: Some((file, text)),
            then: InlineThen::BulkRename { dir, names },
        });
    }

    /// Back from the inline program.
    pub fn inline_done(&mut self, then: InlineThen, r: InlineResult) {
        self.mode = Mode::Normal;
        self.dirty = true;
        match then {
            InlineThen::Refresh { what } => {
                self.refresh();
                match r.status {
                    Ok(0) => self.set_status(format!("back from {what}"), false),
                    Ok(code) => self.set_status(format!("{what} exited with {code}"), true),
                    Err(e) => self.set_status(format!("couldn't run {what}: {e}"), true),
                }
            }
            InlineThen::BulkRename { dir, names } => {
                if let Err(e) = r.status {
                    return self.set_status(format!(":rename: couldn't run the editor: {e}"), true);
                }
                let Some(text) = r.text else {
                    return self.set_status(
                        ":rename: the list was deleted, nothing renamed".into(),
                        true,
                    );
                };
                let pairs = match ops::plan_bulk(&names, &text) {
                    Ok(p) => p,
                    Err(e) => return self.set_status(format!(":rename: {e}"), true),
                };
                if pairs.is_empty() {
                    return self.set_status(":rename: no names changed".into(), false);
                }
                let preview: Vec<String> = pairs
                    .iter()
                    .take(3)
                    .map(|(a, b)| format!("{a} → {b}"))
                    .collect();
                let more = if pairs.len() > 3 {
                    format!(" (+{} more)", pairs.len() - 3)
                } else {
                    String::new()
                };
                let base = dir.display();
                let base = base.trim_end_matches('\\');
                let full: Vec<(String, String)> = pairs
                    .iter()
                    .map(|(a, b)| (format!("{base}\\{a}"), format!("{base}\\{b}")))
                    .collect();
                let admin = self.admin_prefix().unwrap_or_default();
                self.confirm = Some(Confirm::FileOp {
                    op: FileOp::BulkRename(full),
                    why: format!("{admin}rename {}{more}?", preview.join(", ")),
                });
                self.open_prompt_with(PromptKind::Confirm, "");
            }
        }
    }
}

/// Leaving and re-entering the TUI around an inline program (main.rs implements it).
pub trait Suspend {
    /// Stop reading keys, leave raw mode and the alternate screen.
    fn suspend(&self);
    /// Back into raw mode and the alternate screen, reading keys again.
    fn resume(&self);
}

/// Runs `req` in this terminal: writes its file, suspends the TUI, waits for the program,
/// resumes, reads the file back (and removes it). Blocks while the program runs: the TUI
/// is suspended then, so nothing else needs the UI thread.
pub fn run_inline(req: &Inline, term: &dyn Suspend) -> InlineResult {
    if let Some((path, text)) = &req.file
        && let Err(e) = std::fs::write(path, text)
    {
        return InlineResult {
            status: Err(format!("writing {}: {e}", path.display())),
            text: None,
        };
    }
    term.suspend();
    let status = std::process::Command::new(&req.program)
        .args(&req.args)
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .map_err(|e| format!("{}: {e}", req.program));
    term.resume();
    let text = req.file.as_ref().and_then(|(path, _)| {
        let t = std::fs::read_to_string(path).ok();
        let _ = std::fs::remove_file(path);
        t
    });
    InlineResult { status, text }
}
