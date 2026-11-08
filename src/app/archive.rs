//! Zip and extract from the explorer: `<leader>zc` / `<leader>zx`, `:zip [name]`,
//! `:unzip`, and "Compress to zip" / "Extract here" in the `O` menu. This is the asking
//! (what, under which name, the admin-share confirmation); the work is a job, see
//! `jobs::archive`.

use super::keys::PromptKind;
use super::tags::Confirm;
use super::{App, Loc};
use crate::fs::EntryKind;
use crate::jobs::archive::{self, ArchiveJob, ExtractTo, Format};
use crate::jobs::robocopy::{Overwrite, Step};
use crate::jobs::zip::{BackupState, StepSet};
use crate::jobs::{self, Backend, Job, Op, Plan};

/// What `<leader>zc` will zip, while its name is asked.
#[derive(Debug, Clone)]
pub struct ZipDraft {
    /// Full paths.
    items: Vec<String>,
    dest_dir: String,
}

/// `<leader>zx` while the folder it extracts into is read, then asked about.
#[derive(Debug, Clone)]
pub struct ExtractAsk {
    token: u64,
    archive: String,
    dest_dir: String,
    seven: Option<std::path::PathBuf>,
    /// The folder, named by the archive (once read).
    folder: String,
}

impl ExtractAsk {
    fn job(&self, to: ExtractTo) -> ArchiveJob {
        ArchiveJob::Extract {
            archive: self.archive.clone(),
            dest_dir: self.dest_dir.clone(),
            to,
            seven: self.seven.clone(),
            log: String::new(),
        }
    }
}

fn name_of(path: &str) -> &str {
    path.trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(path)
}

impl App {
    /// `<leader>zc` / `:zip [name]`: the selection (or `count` items from the cursor)
    /// into a zip in this folder. Without a name, the name is asked (suggested).
    pub(super) fn compress_selection(&mut self, name: &str, count: usize) {
        let Some(dir) = self.cwd.as_fs().map(|p| p.display()) else {
            return self.set_status(
                "zip works inside a folder (not in Locations or search results)".into(),
                false,
            );
        };
        let Some(listing) = self.view_listing.clone() else {
            return;
        };
        let items: Vec<(String, bool)> = self
            .operand(count)
            .into_iter()
            .filter_map(|i| listing.entries.get(i))
            .filter(|e| matches!(e.kind, EntryKind::File | EntryKind::Dir))
            .filter_map(|e| match self.cwd.join(&e.name) {
                Ok(Loc::Fs(p)) => Some((p.display(), e.kind == EntryKind::Dir)),
                _ => None,
            })
            .collect();
        self.visual = None;
        self.compress_paths(items, dir, name);
    }

    /// Zip these (full path, is a folder) into `dest_dir`.
    pub(super) fn compress_paths(
        &mut self,
        items: Vec<(String, bool)>,
        dest_dir: String,
        name: &str,
    ) {
        if items.is_empty() {
            return self.set_status("nothing to zip here".into(), false);
        }
        self.marked.clear();
        let names: Vec<(String, bool)> = items
            .iter()
            .map(|(p, d)| (name_of(p).to_string(), *d))
            .collect();
        let suggestion = archive::suggested_zip(&names, name_of(&dest_dir));
        self.zip_draft = Some(ZipDraft {
            items: items.into_iter().map(|(p, _)| p).collect(),
            dest_dir,
        });
        if name.trim().is_empty() {
            self.open_prompt_with(PromptKind::ZipName, &suggestion);
        } else if let Err(e) = self.zip_named(name) {
            self.zip_draft = None;
            self.set_status(e, true);
        }
    }

    /// Under the zip prompt: what it will make, or what's wrong with the name.
    pub(super) fn zip_note(&self, text: &str) -> String {
        let Some(d) = &self.zip_draft else {
            return String::new();
        };
        let name = match archive::zip_file_name(text) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let what = match d.items.as_slice() {
            [one] => name_of(one).to_string(),
            many => format!("{} items", many.len()),
        };
        let taken = self
            .view_listing
            .as_ref()
            .is_some_and(|l| l.entries.iter().any(|e| e.name.eq_ignore_ascii_case(&name)));
        if taken {
            format!("{what} → {name} (exists: the new zip gets a number)")
        } else {
            format!("{what} → {name}")
        }
    }

    /// Enter on the zip prompt (or `:zip name`).
    pub(super) fn zip_named(&mut self, text: &str) -> Result<(), String> {
        let name = archive::zip_file_name(text)?;
        let Some(d) = self.zip_draft.take() else {
            return Ok(());
        };
        self.confirm_archive(ArchiveJob::Compress {
            items: d.items,
            dest_dir: d.dest_dir,
            zip_name: name,
            log: String::new(),
        });
        Ok(())
    }

    pub(super) fn zip_cancel(&mut self) {
        self.zip_draft = None;
        self.set_status("not zipped".into(), false);
    }

    /// `<leader>zx` / `:unzip`: the archive under the cursor, extracted here.
    pub(super) fn extract_selected(&mut self) {
        let Some(e) = self.selected().cloned() else {
            return self.set_status("nothing under the cursor".into(), false);
        };
        let path = match (e.kind, self.cwd.join(&e.name)) {
            (EntryKind::File, Ok(Loc::Fs(p))) => p.display(),
            _ => {
                return self.set_status(
                    "put the cursor on an archive (.zip; .7z .rar .tar.gz with 7-Zip)".into(),
                    false,
                );
            }
        };
        self.extract_path(path);
    }

    /// Extract `path` into the folder it's in.
    pub(super) fn extract_path(&mut self, path: String) {
        let name = name_of(&path).to_string();
        let seven = match archive::format_of(&name) {
            None => {
                return self.set_status(
                    format!("{name} isn't an archive (.zip; .7z .rar .tar.gz with 7-Zip)"),
                    false,
                );
            }
            Some(Format::Zip) => None,
            Some(Format::SevenZip) => match &self.seven_zip {
                Some(s) => Some(std::path::PathBuf::from(s)),
                None => {
                    return self.set_status(
                        format!("{name}: this kind of archive needs 7-Zip (7z.exe), not found"),
                        true,
                    );
                }
            },
        };
        let Some(dest_dir) = jobs::parent_of(&path).map(str::to_string) else {
            return self.set_status(format!("no folder to extract {name} into"), true);
        };
        // First the folder it extracts into, named by the archive (read off-thread), and
        // whether it's there: if so, ask.
        self.extract_token += 1;
        let token = self.extract_token;
        self.extract_ask = Some(ExtractAsk {
            token,
            archive: path.clone(),
            dest_dir: dest_dir.clone(),
            seven: seven.clone(),
            folder: String::new(),
        });
        self.set_status(format!("reading {name}…"), false);
        self.worker.archive_target(token, path, dest_dir, seven);
    }

    /// The worker named the folder: extract into it if it's new, else ask.
    pub(super) fn on_archive_target(&mut self, token: u64, result: Result<(String, bool), String>) {
        let Some(mut ask) = self.extract_ask.take().filter(|a| a.token == token) else {
            return;
        };
        let (folder, exists) = match result {
            Ok(r) => r,
            Err(e) => return self.set_status(e, true),
        };
        if !exists {
            return self.confirm_archive(ask.job(ExtractTo::New(folder)));
        }
        ask.folder = folder;
        self.extract_ask = Some(ask);
        self.open_prompt_with(PromptKind::ExtractExists, "");
        // A new folder is the default: Enter alone never writes into an existing one.
        if let Some(p) = self.prompt.as_mut() {
            p.yes = false;
        }
    }

    /// The popup when the folder exists.
    pub(super) fn extract_dialog(&self, merge: bool) -> Option<super::DialogView> {
        let a = self.extract_ask.as_ref()?;
        let new = self.free_folder_here(&a.folder, &a.dest_dir);
        Some(super::DialogView {
            title: "Extract".to_string(),
            message: format!(
                "{} already exists here. Merge into it (files with the same name are \
                 replaced, nothing else is touched), or extract into a new folder {new}?",
                a.folder
            ),
            items: vec![name_of(&a.archive).to_string()],
            danger: false,
            yes: merge,
            buttons: ["Merge", "New folder"],
            keys: "m merge · n new folder · Esc cancel · h/l Tab · Enter",
        })
    }

    /// `aveva_1` (what the job will most likely pick), from the folder shown when it's
    /// the one extracted into.
    fn free_folder_here(&self, folder: &str, dest_dir: &str) -> String {
        let here = self
            .cwd
            .as_fs()
            .is_some_and(|p| p.display().eq_ignore_ascii_case(dest_dir));
        let taken: std::collections::HashSet<String> = match (&self.view_listing, here) {
            (Some(l), true) => l.entries.iter().map(|e| e.name.to_lowercase()).collect(),
            _ => [folder.to_lowercase()].into(),
        };
        archive::free_folder(folder, &taken)
    }

    /// The answer: merge (Some(true)), a new folder (Some(false)), or cancel (None).
    pub(super) fn extract_answer(&mut self, merge: Option<bool>) {
        let Some(a) = self.extract_ask.take() else {
            return;
        };
        match merge {
            None => self.set_status("not extracted".into(), false),
            Some(true) => self.confirm_archive(a.job(ExtractTo::Merge(a.folder.clone()))),
            Some(false) => self.confirm_archive(a.job(ExtractTo::New(a.folder.clone()))),
        }
    }

    /// On an admin share, a y/n naming the machine first; elsewhere straight on.
    fn confirm_archive(&mut self, job: ArchiveJob) {
        match self.admin_prefix() {
            Some(prefix) => {
                let why = format!("{prefix}{}.", job.describe());
                self.confirm = Some(Confirm::Archive {
                    job: Box::new(job),
                    why,
                });
                self.open_prompt_with(PromptKind::Confirm, "");
            }
            None => self.start_archive(job),
        }
    }

    /// Starts the job and lists it in the panel.
    pub(super) fn start_archive(&mut self, mut job: ArchiveJob) {
        let id = self.new_job_id();
        let (title, kind, dest, step) = match &job {
            ArchiveJob::Compress {
                items,
                dest_dir,
                zip_name,
                ..
            } => {
                // The panel's PATHS show where it reads from.
                let files = items.iter().map(|p| name_of(p).to_string()).collect();
                (
                    zip_name.clone(),
                    StepSet::Compress,
                    format!("{}\\{zip_name}", dest_dir.trim_end_matches('\\')),
                    Step {
                        src: dest_dir.clone(),
                        dst: dest_dir.clone(),
                        files,
                    },
                )
            }
            ArchiveJob::Extract {
                archive,
                dest_dir,
                to,
                ..
            } => {
                let name = name_of(archive).to_string();
                let (ExtractTo::New(folder) | ExtractTo::Merge(folder)) = to;
                (
                    name.clone(),
                    StepSet::Extract,
                    format!("{}\\{folder}", dest_dir.trim_end_matches('\\')),
                    Step {
                        src: dest_dir.clone(),
                        dst: dest_dir.clone(),
                        files: vec![name],
                    },
                )
            }
        };
        let log = jobs::log_path(id, &title).unwrap_or_else(|| format!("sentier-job-{id}.log"));
        match &mut job {
            ArchiveJob::Compress { log: l, .. } | ArchiveJob::Extract { log: l, .. } => {
                *l = log.clone();
            }
        }
        let plan = Plan {
            backend: Backend::Native,
            steps: vec![step],
            pairs: Vec::new(),
            title: title.clone(),
        };
        let mut j = Job::new(id, &plan, Op::Copy, dest, Overwrite::Changed, log);
        j.backup = Some(BackupState {
            kind,
            step: kind.steps().first().copied(),
            ..BackupState::default()
        });
        let audit = if self.config.machines.audit.unwrap_or(true) {
            self.admin_target(&self.cwd)
        } else {
            None
        };
        let msg = format!("{} · J shows jobs", job.describe());
        let cancel = self.worker.start_archive(id, job, audit);
        self.push_job(j, cancel);
        self.set_status(msg, false);
    }
}
