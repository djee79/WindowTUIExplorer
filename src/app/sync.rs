//! Sync presets (CLAUDE.md section 9): a `kind = "sync"` pin refreshes a local copy of a
//! server folder.
//!
//! - Opening one (Enter, `'key`, `:sync <name>`, `<leader>rs`) runs a dry run first
//!   (`robocopy /MIR /L`) and shows the preview (docs/screens/05-sync-preview.txt):
//!   what will be copied, replaced and, in mirror mode, DELETED locally.
//! - In the preview: `Enter` runs it, `u` switches mirror ↔ update for this run, `Tab`
//!   lists every change (`j`/`k`, `^d`/`^u`, `gg`/`G` scroll), `R` runs the dry run again,
//!   `Esc`/`q` cancels (nothing was touched).
//! - `Shift-Enter` in a picker or `:sync! <name>` skips the preview for an update-mode
//!   preset. Mirror always previews.
//! - The real run is a normal job (Finishing phase, report, log); when it ends its
//!   outcome goes to `presets.json` and shows on the pin as `last: today 08:12 ✓ …`.

use super::{App, Mode};
use crate::fs::worker::CancelToken;
use crate::jobs::preset::{Change, LastRun, PresetRef, Preview, Row, SyncMode, SyncPreset};
use crate::jobs::robocopy::{Options, Overwrite, Tracker};
use crate::jobs::run::Spec;
use crate::jobs::{self, Backend, Job, Op, Plan};
use crate::pins::PinKind;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

/// Rows `^d` / `^u` move in the preview's list.
const HALF_PAGE: usize = 10;

#[derive(Debug, Clone)]
pub enum PreviewStatus {
    Running,
    Ready(Box<Preview>),
    Failed(String),
}

#[derive(Debug)]
pub struct SyncPreviewState {
    pub preset: SyncPreset,
    /// The mode this run will use (`u` flips it; the pin keeps its own).
    pub mode: SyncMode,
    pub status: PreviewStatus,
    pub started: Instant,
    /// How long the dry run took.
    pub took: Option<Duration>,
    /// `Tab`: every change, not just the deletions (mirror) or replacements (update).
    pub show_all: bool,
    pub scroll: usize,
    token: u64,
    cancel: CancelToken,
    pending_g: bool,
}

impl SyncPreviewState {
    pub fn preview(&self) -> Option<&Preview> {
        match &self.status {
            PreviewStatus::Ready(p) => Some(p),
            _ => None,
        }
    }

    /// The list under the counts: everything (`Tab`), else what deserves a look before
    /// running: the deletions in mirror mode, the replaced local files in update mode.
    pub fn listed(&self) -> (String, Vec<&Row>) {
        let Some(pv) = self.preview() else {
            return (String::new(), Vec::new());
        };
        let rows: Vec<&Row> = if self.show_all {
            pv.rows.iter().collect()
        } else {
            let want = match self.mode {
                SyncMode::Mirror => Change::Extra,
                SyncMode::Update => Change::Replace,
            };
            pv.rows.iter().filter(|r| r.change == want).collect()
        };
        let title = match (self.show_all, self.mode) {
            (true, _) => "Every change",
            (false, SyncMode::Mirror) => "Will be deleted",
            (false, SyncMode::Update) => "Local copies replaced",
        };
        (format!("{title} ({})", rows.len()), rows)
    }
}

impl App {
    /// The dry run always mirrors, so it lists the local-only files in both modes.
    fn dry_run_options(preset: &SyncPreset) -> Options {
        Options {
            moving: false,
            overwrite: Overwrite::Changed,
            mirror: true,
            list_only: true,
            threads: 0,
            restartable: false,
            log: None,
            exclude_dirs: preset.exclude_dirs.clone(),
            exclude_files: preset.exclude_files.clone(),
        }
    }

    /// Enter / `'key` / `:sync` on a sync pin. `skip_preview` (Shift-Enter, `:sync!`)
    /// runs an update-mode preset at once; mirror previews anyway.
    pub(super) fn open_sync_pin(&mut self, idx: usize, skip_preview: bool) {
        let Some(pin) = self.pins.pins.get(idx) else {
            return;
        };
        let preset = match SyncPreset::from_pin(pin) {
            Ok(p) => p,
            Err(e) => return self.set_status(format!("{}: {e}", pin.name), true),
        };
        if skip_preview && preset.mode == SyncMode::Update {
            return self.run_sync(preset, SyncMode::Update, None);
        }
        let mode = preset.mode;
        self.start_dry_run(preset, mode);
        if skip_preview {
            self.set_status("mirror always previews first".into(), false);
        }
    }

    fn start_dry_run(&mut self, preset: SyncPreset, mode: SyncMode) {
        if let Some(old) = self.sync_preview.take() {
            old.cancel.cancel();
        }
        self.sync_token += 1;
        let token = self.sync_token;
        let cancel =
            self.worker
                .sync_preview(token, preset.steps(), Self::dry_run_options(&preset));
        self.sync_preview = Some(SyncPreviewState {
            preset,
            mode,
            status: PreviewStatus::Running,
            started: Instant::now(),
            took: None,
            show_all: false,
            scroll: 0,
            token,
            cancel,
            pending_g: false,
        });
        self.mode = Mode::SyncPreview;
        self.dirty = true;
    }

    pub(super) fn on_sync_previewed(
        &mut self,
        token: u64,
        result: Result<(Box<Tracker>, i32), String>,
    ) {
        let Some(sp) = self.sync_preview.as_mut() else {
            return;
        };
        if sp.token != token {
            return;
        }
        sp.took = Some(sp.started.elapsed());
        sp.status = match result {
            Ok((t, code)) => {
                let pv = Preview::from_tracker(&t, code, &sp.preset.step());
                match pv.problem() {
                    Some(p) => PreviewStatus::Failed(p),
                    None => PreviewStatus::Ready(Box::new(pv)),
                }
            }
            Err(e) => PreviewStatus::Failed(e),
        };
        let msg = match &sp.status {
            PreviewStatus::Ready(pv) => Some((
                format!("{}: {}", sp.preset.name, pv.summary(sp.mode)),
                false,
            )),
            PreviewStatus::Failed(e) => Some((format!("dry run failed: {e}"), true)),
            PreviewStatus::Running => None,
        };
        if let Some((text, err)) = msg {
            self.set_status(text, err);
        }
        self.dirty = true;
    }

    fn close_sync_preview(&mut self) {
        if let Some(sp) = self.sync_preview.take() {
            sp.cancel.cancel();
        }
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    pub(super) fn on_sync_preview_key(&mut self, ev: KeyEvent) {
        let Some(sp) = self.sync_preview.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let len = sp.listed().1.len();
        let last = len.saturating_sub(1);
        let g = std::mem::take(&mut sp.pending_g);
        match ev.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close_sync_preview();
                self.set_status("sync cancelled: nothing was touched".into(), false);
            }
            KeyCode::Char('u') if !ctrl => {
                sp.mode = sp.mode.flip();
                sp.scroll = 0;
            }
            KeyCode::Tab => {
                sp.show_all = !sp.show_all;
                sp.scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down if !ctrl => sp.scroll = (sp.scroll + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => sp.scroll = sp.scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => sp.scroll = (sp.scroll + HALF_PAGE).min(last),
            KeyCode::Char('u') if ctrl => sp.scroll = sp.scroll.saturating_sub(HALF_PAGE),
            KeyCode::Char('G') => sp.scroll = last,
            KeyCode::Char('g') if g => sp.scroll = 0,
            KeyCode::Char('g') => sp.pending_g = true,
            KeyCode::Char('R') => {
                let (preset, mode) = (sp.preset.clone(), sp.mode);
                self.start_dry_run(preset, mode);
            }
            KeyCode::Char('x') if !ctrl => self.exclude_row(false),
            KeyCode::Char('X') => self.exclude_row(true),
            KeyCode::Char('E') => self.clear_exclusions(),
            KeyCode::Enter => self.confirm_sync(),
            _ => {}
        }
    }

    /// The dry run again (the preset changed), on the same list and row.
    fn redo_dry_run(&mut self, preset: SyncPreset, mode: SyncMode) {
        let (show_all, row) = self
            .sync_preview
            .as_ref()
            .map_or((false, 0), |s| (s.show_all, s.scroll));
        self.start_dry_run(preset, mode);
        if let Some(sp) = self.sync_preview.as_mut() {
            sp.show_all = show_all;
            sp.scroll = row;
        }
    }

    /// The pin a preview's preset came from (found by its folders: names can repeat).
    fn preset_pin_index(&self, preset: &SyncPreset) -> Option<usize> {
        let key = preset.key();
        self.pins
            .pins
            .iter()
            .position(|p| SyncPreset::from_pin(p).is_ok_and(|s| s.key() == key))
    }

    /// `x` / `X` in the preview: leave the file under the cursor (or, `X`, the folder it's
    /// in) out of this preset from now on: robocopy `/XF` / `/XD` with its full path, so
    /// it's neither copied nor, in mirror mode, deleted. Saved, and the dry run redone.
    fn exclude_row(&mut self, folder: bool) {
        let Some(sp) = self.sync_preview.as_ref() else {
            return;
        };
        let (_, rows) = sp.listed();
        let Some(row) = rows.get(sp.scroll).copied().cloned() else {
            return self.set_status("nothing under the cursor to leave out".into(), false);
        };
        let base = if row.change == Change::Extra {
            sp.preset.dest.display()
        } else {
            sp.preset.source.display()
        };
        let base = base.trim_end_matches('\\').to_string();
        let (key, path, what) = if folder {
            let Some((parent, _)) = row.rel.rsplit_once('\\') else {
                return self.set_status(
                    "it's in the top folder: x leaves the file out".into(),
                    false,
                );
            };
            (
                "exclude_dirs",
                format!("{base}\\{parent}"),
                format!("{parent}\\"),
            )
        } else {
            (
                "exclude_files",
                format!("{base}\\{}", row.rel),
                row.rel.clone(),
            )
        };
        let (preset, mode) = (sp.preset.clone(), sp.mode);
        let Some(idx) = self.preset_pin_index(&preset) else {
            return self.set_status("this preset isn't a pin any more".into(), true);
        };
        let Some(pin) = self.pins.pins.get_mut(idx) else {
            return;
        };
        let mut list: Vec<String> = pin
            .extra
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !list.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
            list.push(path);
        }
        pin.extra.insert(
            key.into(),
            toml::Value::Array(list.into_iter().map(toml::Value::String).collect()),
        );
        let Ok(preset2) = SyncPreset::from_pin(pin) else {
            return;
        };
        self.save_pins();
        self.redo_dry_run(preset2, mode);
        self.set_status(
            format!(
                "{what} left out of {} from now on (E brings everything back)",
                preset.name
            ),
            false,
        );
    }

    /// `E` in the preview: no exclusions any more (made with `x` / `X`, or in pins.toml).
    fn clear_exclusions(&mut self) {
        let Some(sp) = self.sync_preview.as_ref() else {
            return;
        };
        if sp.preset.exclude_dirs.is_empty() && sp.preset.exclude_files.is_empty() {
            return self.set_status("nothing is left out of this preset".into(), false);
        }
        let (preset, mode) = (sp.preset.clone(), sp.mode);
        let Some(idx) = self.preset_pin_index(&preset) else {
            return self.set_status("this preset isn't a pin any more".into(), true);
        };
        let Some(pin) = self.pins.pins.get_mut(idx) else {
            return;
        };
        pin.extra.remove("exclude_dirs");
        pin.extra.remove("exclude_files");
        let Ok(preset2) = SyncPreset::from_pin(pin) else {
            return;
        };
        self.save_pins();
        self.redo_dry_run(preset2, mode);
        self.set_status(
            "nothing left out any more: everything is synced".into(),
            false,
        );
    }

    /// Enter in the preview: run it, unless there's nothing to do or the dry run failed.
    fn confirm_sync(&mut self) {
        let Some(sp) = self.sync_preview.as_ref() else {
            return;
        };
        match &sp.status {
            PreviewStatus::Running => {
                self.set_status("the dry run is still going…".into(), false);
            }
            PreviewStatus::Failed(e) => {
                let msg = format!("can't sync: {e} (R tries again)");
                self.set_status(msg, true);
            }
            PreviewStatus::Ready(pv) => {
                let (preset, mode) = (sp.preset.clone(), sp.mode);
                if pv.is_noop(mode) {
                    let name = preset.dest.name();
                    self.close_sync_preview();
                    return self.set_status(format!("{name} is up to date: nothing to do"), false);
                }
                let totals = pv.to_copy();
                self.close_sync_preview();
                self.run_sync(preset, mode, Some(totals));
            }
        }
    }

    /// Starts the real run as a job. `totals` from the preview skip robocopy's `/L` pass.
    pub(super) fn run_sync(
        &mut self,
        preset: SyncPreset,
        mode: SyncMode,
        totals: Option<(u64, u64)>,
    ) {
        let id = self.new_job_id();
        let plan = Plan {
            backend: Backend::Robocopy,
            steps: preset.steps(),
            pairs: Vec::new(),
            title: preset.name.clone(),
        };
        let log =
            jobs::log_path(id, &plan.title).unwrap_or_else(|| format!("sentier-job-{id}.log"));
        let mut job = Job::new(
            id,
            &plan,
            Op::Copy,
            preset.dest.display(),
            Overwrite::Changed,
            log.clone(),
        );
        job.preset = Some(PresetRef {
            key: preset.key(),
            label: SyncPreset::label(mode),
        });
        let spec = Spec {
            plan,
            moving: false,
            overwrite: Overwrite::Changed,
            threads: self.config.jobs.threads(),
            restartable: preset.restartable,
            log,
            mirror: mode == SyncMode::Mirror,
            exclude_dirs: preset.exclude_dirs.clone(),
            exclude_files: preset.exclude_files.clone(),
            totals,
        };
        self.launch_job(job, spec);
        self.set_status(
            format!("sync {} ({}) · J shows jobs", preset.name, mode.word()),
            false,
        );
    }

    /// A preset job ended: remember how, here and in `presets.json`.
    pub(super) fn record_preset_run(&mut self, key: &str, run: LastRun) {
        self.preset_runs.insert(key.to_string(), run.clone());
        self.worker.record_preset_run(key.to_string(), run);
    }

    /// `last: today 08:12 ✓ 3.80 GB in 2m41s` for a sync or backup pin that has run.
    pub fn last_run_line(&self, pin: &crate::pins::Pin) -> Option<String> {
        let key = match pin.kind {
            PinKind::Sync => SyncPreset::from_pin(pin).ok()?.key(),
            PinKind::Backup => crate::jobs::zip::BackupPreset::from_pin(pin).ok()?.key(),
            _ => return None,
        };
        let run = self.preset_runs.get(&key)?;
        Some(run.line(chrono::Local::now()))
    }

    /// `:sync [name]` / `:sync! [name]`: by name (exact, else the one containing it), or
    /// the sync preset picker.
    pub(super) fn sync_command(&mut self, name: &str, skip_preview: bool) {
        self.preset_command(PinKind::Sync, name, skip_preview);
    }

    /// `:sync` / `:backup`: finds the preset by name and opens it; no name, an unknown
    /// one or several matches open the picker filtered to that kind.
    pub(super) fn preset_command(&mut self, kind: PinKind, name: &str, skip_preview: bool) {
        let name = name.trim();
        let word = kind.label();
        if name.is_empty() {
            return self.open_pin_picker(&format!("is:{word} "));
        }
        if name.eq_ignore_ascii_case("new") {
            return self.new_preset(kind);
        }
        let syncs: Vec<usize> = (0..self.pins.pins.len())
            .filter(|&i| self.pins.pins[i].kind == kind)
            .collect();
        let lower = name.to_lowercase();
        let exact = syncs
            .iter()
            .copied()
            .find(|&i| self.pins.pins[i].name.to_lowercase() == lower);
        let found = exact.map(|i| vec![i]).unwrap_or_else(|| {
            syncs
                .iter()
                .copied()
                .filter(|&i| self.pins.pins[i].name.to_lowercase().contains(&lower))
                .collect()
        });
        match found.as_slice() {
            [i] if kind == PinKind::Sync => self.open_sync_pin(*i, skip_preview),
            [i] => self.open_backup_pin(*i),
            [] if syncs.is_empty() => self.set_status(
                format!("no {word} presets yet (kind = \"{word}\" pins in pins.toml)"),
                true,
            ),
            [] => self.set_status(format!("no {word} preset matches {name:?}"), true),
            _ => self.open_pin_picker(&format!("is:{word} {name}")),
        }
    }
}
