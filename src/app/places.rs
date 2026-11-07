//! The places menu: `O` (or `^o`) on a sync / backup preset in a picker, or on a job in
//! the jobs panel. A preset has two folders, so the menu offers both: go to the source or
//! the destination, both at once (the destination in a new tab), either or both in
//! Explorer, or copy a path. Drawn by the `O` menu (`ui/openwith.rs`).

use super::open::OpenMenu;
use super::{App, JobRow, Mode};
use crate::fs::path::Loc;
use crate::open::{MenuItem, Section};
use crate::pins::{Pin, PinKind};

/// A preset's or job's two folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    pub source: String,
    pub dest: String,
}

impl Places {
    /// A sync preset (source → dest) or a backup preset (source → zips folder).
    pub fn of_pin(pin: &Pin) -> Result<Places, String> {
        match pin.kind {
            PinKind::Sync => crate::jobs::preset::SyncPreset::from_pin(pin).map(|p| Places {
                source: p.source.display(),
                dest: p.dest.display(),
            }),
            PinKind::Backup => crate::jobs::zip::BackupPreset::from_pin(pin).map(|p| Places {
                source: p.source.display(),
                dest: p.dest_dir.display(),
            }),
            _ => Err(format!("{} has one place: Enter opens it", pin.name)),
        }
    }
}

fn item(id: &str, label: &str, detail: &str, section: Section) -> MenuItem {
    MenuItem {
        id: id.to_string(),
        label: label.to_string(),
        detail: detail.to_string(),
        hint: String::new(),
        missing: false,
        last: false,
        section,
    }
}

/// The rows: in sentier, in Explorer, copy.
pub fn menu_items(p: &Places) -> Vec<MenuItem> {
    let (s, d) = (p.source.as_str(), p.dest.as_str());
    if s.is_empty() {
        // A job that kept no source folder: the destination only.
        return vec![
            item("go-dst", "Destination", d, Section::Configured),
            item("x-dst", "Destination", d, Section::Windows),
            item("c-dst", "Destination path", d, Section::System),
        ];
    }
    vec![
        item("go-src", "Source", s, Section::Configured),
        item("go-dst", "Destination", d, Section::Configured),
        item(
            "both",
            "Both",
            "source here, destination in a new tab",
            Section::Configured,
        ),
        item("x-src", "Source", s, Section::Windows),
        item("x-dst", "Destination", d, Section::Windows),
        item("x-both", "Both", "two Explorer windows", Section::Windows),
        item("c-src", "Source path", s, Section::System),
        item("c-dst", "Destination path", d, Section::System),
    ]
}

impl App {
    /// Opens the places menu for `title` (a preset's or job's name).
    pub(super) fn open_places(&mut self, title: String, places: Places) {
        self.open_menu = Some(OpenMenu::for_places(title, places));
        self.mode = Mode::OpenWith;
        self.dirty = true;
    }

    /// `O` / `^o` on a picker row: a sync / backup preset's places. False when the row
    /// isn't one (the caller does what it did before).
    pub(super) fn picker_places(&mut self) -> bool {
        let Some(item) = self.picker.as_ref().and_then(|p| p.selected()).cloned() else {
            return false;
        };
        let crate::picker::ItemKind::Pin(kind) = item.kind else {
            return false;
        };
        if !matches!(kind, PinKind::Sync | PinKind::Backup) {
            return false;
        }
        let Some(pin) = self.pins.pins.get(item.id).cloned() else {
            return false;
        };
        match Places::of_pin(&pin) {
            Ok(places) => {
                self.close_picker();
                self.open_places(pin.name.clone(), places);
            }
            Err(e) => self.set_status(format!("{}: {e}", pin.name), true),
        }
        true
    }

    /// `O` in the jobs panel: the selected job's source and destination.
    pub(super) fn job_places(&mut self) {
        // A backup's dest is the zip: its folder is the place. The source is the first
        // robocopy step's folder (the job copied it, or files out of it).
        let folder_of_zip = |dest: &str, backup: bool| {
            if backup {
                crate::jobs::parent_of(dest).unwrap_or(dest).to_string()
            } else {
                dest.to_string()
            }
        };
        let found = match self.selected_job_row() {
            Some(JobRow::Live(i)) => self.jobs.get(i).map(|j| {
                (
                    j.title.clone(),
                    j.steps.first().map(|s| s.src.clone()),
                    folder_of_zip(&j.dest, j.backup.is_some()),
                )
            }),
            Some(JobRow::Old(i)) => self.old_reports.get(i).map(|r| {
                (
                    r.title.clone(),
                    r.steps.first().map(|s| s.src.clone()),
                    folder_of_zip(&r.dest, r.backup.is_some()),
                )
            }),
            None => None,
        };
        let Some((title, source, dest)) = found else {
            return self.set_status("no job here".into(), false);
        };
        // Small copies (one file at a time) keep no source folder: only the destination.
        let source = source.unwrap_or_default();
        self.open_places(title, Places { source, dest });
    }

    /// A row of the places menu.
    pub(super) fn run_place(&mut self, id: &str, p: Places) {
        let go = |app: &mut App, path: &str| match Loc::parse(path) {
            Ok(loc) => app.navigate(loc, None),
            Err(e) => app.set_status(format!("{path}: {e}"), true),
        };
        let explorer = |app: &mut App, path: &str| {
            app.worker.run_detached(
                format!("Explorer: {path}"),
                "explorer.exe".into(),
                vec![path.to_string()],
                None,
            );
        };
        match id {
            "go-src" => go(self, &p.source),
            "go-dst" => go(self, &p.dest),
            "both" => {
                go(self, &p.source);
                self.new_tab(Some(&p.dest));
            }
            "x-src" => explorer(self, &p.source),
            "x-dst" => explorer(self, &p.dest),
            "x-both" => {
                explorer(self, &p.source);
                explorer(self, &p.dest);
            }
            "c-src" => {
                self.worker
                    .copy_text("copied the source path".into(), vec![p.source], false)
            }
            "c-dst" => {
                self.worker
                    .copy_text("copied the destination path".into(), vec![p.dest], false)
            }
            _ => {}
        }
    }
}
