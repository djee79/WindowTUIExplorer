//! Backup presets (CLAUDE.md section 9): a `kind = "backup"` pin zips a project folder.
//!
//! Enter on one (picker, Locations, `'key`), `:backup <name>` or `<leader>rb` asks once,
//! naming the zip and the retention, then runs the pipeline in `jobs::zip` as a job:
//! staging → zipping → verifying → retention → cleanup, shown step by step in the jobs
//! panel. A source that is a sync preset's `dest` is already a local copy: it's zipped
//! directly, without staging.

use super::App;
use super::keys::PromptKind;
use super::tags::Confirm;
use crate::jobs::robocopy::Overwrite;
use crate::jobs::zip::{BackupPreset, BackupSpec, BackupState, BackupStep, Engine};
use crate::jobs::{self, Backend, Job, Op, Plan};
use crate::pins::PinKind;

impl App {
    /// Whether `preset`'s source is the `dest` of a sync preset (a local copy).
    fn source_is_local_copy(&self, preset: &BackupPreset) -> bool {
        self.pins
            .pins
            .iter()
            .filter(|p| p.kind == PinKind::Sync)
            .filter_map(|p| crate::jobs::preset::SyncPreset::from_pin(p).ok())
            .any(|s| s.dest.starts_with(&preset.source) && preset.source.starts_with(&s.dest))
    }

    /// Enter / `'key` / `:backup` on a backup pin: one question, then the job.
    pub(super) fn open_backup_pin(&mut self, idx: usize) {
        let Some(pin) = self.pins.pins.get(idx) else {
            return;
        };
        let preset = match BackupPreset::from_pin(pin) {
            Ok(p) => p,
            Err(e) => return self.set_status(format!("{}: {e}", pin.name), true),
        };
        let zip = match preset.zip_name(chrono::Local::now()) {
            Ok(z) => z,
            Err(e) => return self.set_status(format!("{}: {e}", preset.name), true),
        };
        let direct = self.source_is_local_copy(&preset);
        let keep = match preset.keep {
            0 => "keeps every zip".to_string(),
            n => format!("keeps the newest {n}"),
        };
        let mut how = if direct {
            "zipped directly: it's a sync preset's local copy".to_string()
        } else {
            "staged locally first".to_string()
        };
        match (preset.engine, &self.seven_zip) {
            (Engine::SevenZip, Some(_)) => how.push_str(", 7-Zip"),
            (Engine::SevenZip, None) => how.push_str(", built-in zip (7-Zip isn't installed)"),
            // The offer: 7-Zip is there and this preset doesn't use it.
            (Engine::Builtin, Some(_)) => {
                how.push_str(", built-in zip; 7-Zip is installed, ^e → engine 7zip is faster")
            }
            (Engine::Builtin, None) => {}
        }
        let why = format!(
            "Back up {} to {}\\{zip}? ({how}; {keep})",
            preset.source.display(),
            preset.dest_dir.display()
        );
        self.confirm = Some(Confirm::Backup {
            preset: Box::new(preset),
            direct,
            why,
        });
        self.open_prompt_with(PromptKind::Confirm, "");
    }

    /// Starts the pipeline as a job.
    pub(super) fn run_backup(&mut self, preset: BackupPreset, direct: bool) {
        let (zip_name, pattern) = match (preset.zip_name(chrono::Local::now()), preset.pattern()) {
            (Ok(z), Ok(p)) => (z, p),
            (Err(e), _) | (_, Err(e)) => return self.set_status(e, true),
        };
        let id = self.new_job_id();
        let staging = if direct {
            None
        } else {
            match jobs::zip::staging_dir(id) {
                Some(s) => Some(s),
                None => return self.set_status("no %LOCALAPPDATA% to stage in".into(), true),
            }
        };
        let log =
            jobs::log_path(id, &preset.name).unwrap_or_else(|| format!("sentier-job-{id}.log"));
        let plan = Plan {
            backend: Backend::Robocopy,
            steps: Vec::new(),
            pairs: Vec::new(),
            title: preset.name.clone(),
        };
        let mut job = Job::new(
            id,
            &plan,
            Op::Copy,
            // The zip's path: the list shows its name, and its folder refreshes at the end.
            preset
                .dest_dir
                .join(&zip_name)
                .map_or_else(|_| preset.dest_dir.display(), |p| p.display()),
            Overwrite::Changed,
            log.clone(),
        );
        job.preset = Some(preset.preset_ref());
        job.backup = Some(BackupState {
            step: Some(BackupStep::Staging),
            direct,
            ..BackupState::default()
        });
        let spec = BackupSpec {
            title: preset.name.clone(),
            source: preset.source.display(),
            staging,
            dest_dir: preset.dest_dir.display(),
            zip_name: zip_name.clone(),
            pattern,
            keep: preset.keep,
            compression: preset.compression,
            engine: preset.engine,
            threads: self.config.jobs.threads(),
            restartable: preset.restartable,
            log,
        };
        let cancel = self.worker.start_backup(id, spec);
        self.push_job(job, cancel);
        self.set_status(
            format!("backup {} → {zip_name} · J shows jobs", preset.name),
            false,
        );
    }

    /// `:backup [name]`, `<leader>rb`.
    pub(super) fn backup_command(&mut self, name: &str) {
        self.preset_command(PinKind::Backup, name, false);
    }
}
