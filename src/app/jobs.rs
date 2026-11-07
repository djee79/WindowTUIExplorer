//! Selection, yank / cut / paste, and the jobs they start (CLAUDE.md section 4).
//!
//! - `Tab` toggles the item under the cursor and moves down (with a count: n items).
//!   `v` / `V` start visual mode: the range from where it started to the cursor, moved
//!   with any motion; `y` yanks, `x` cuts, `d` / `D` delete it, `Esc` / `v` leave. Plain `Esc` clears the
//!   selection.
//! - `yy` / `x` put the selection (or the visual range, or `n` items from the cursor)
//!   in the register as a copy / a move. `p` pastes into the current folder without
//!   replacing anything; `P` replaces what's there (asks first). A cut is pasted once.
//! - A paste is a job: robocopy for folders and big files, native copies for small files,
//!   a rename for a move on one volume. Folders that look like an AVEVA project ask first.
//! - `J` (or `<leader>j`) shows the jobs: `j`/`k` pick, `c` cancels, `r` retries the
//!   failed files, `o` opens the log, `Enter`/`dd` dismisses a finished one. The panel
//!   opens by itself once a job runs longer than 5 s.

use super::keys::PromptKind;
use super::tags::Confirm;
use super::{App, Mode};
use crate::fs::EntryKind;
use crate::fs::path::Loc;
use crate::jobs::robocopy::Overwrite;
use crate::jobs::run::Spec;
use crate::jobs::{self, Backend, Item, Job, JobEvent, Op, Phase, Plan, Report, Request};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Instant, SystemTime};

/// What `yy` / `x` put aside for `p`.
#[derive(Debug, Clone)]
pub struct Register {
    pub op: Op,
    pub items: Vec<Item>,
}

/// One row of the jobs panel: a job of this session, or a report kept from an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobRow {
    Live(usize),
    Old(usize),
}

/// A paste waiting for the AVEVA check.
#[derive(Debug, Clone)]
pub struct PendingPaste {
    pub token: u64,
    pub req: Request,
    pub plan: Plan,
}

fn items_word(n: usize) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{n} items")
    }
}

impl App {
    // -------------------------------------------------------------- selection

    /// Entry indices (into the listing) the next operator works on, in view order: the
    /// visual range, else the selection, else `n` items from the cursor.
    pub fn operand(&self, count: usize) -> Vec<usize> {
        if let Some(anchor) = self.visual {
            let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
            return self.view.iter().skip(a).take(b - a + 1).copied().collect();
        }
        if !self.marked.is_empty() {
            let Some(listing) = &self.view_listing else {
                return Vec::new();
            };
            return self
                .view
                .iter()
                .copied()
                .filter(|&i| self.marked.contains(&listing.entries[i].name))
                .collect();
        }
        self.view
            .iter()
            .skip(self.cursor)
            .take(count.max(1))
            .copied()
            .collect()
    }

    /// The row is selected or inside the visual range.
    pub fn is_marked(&self, row: usize, name: &str) -> bool {
        if let Some(anchor) = self.visual {
            let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
            if (a..=b).contains(&row) {
                return true;
            }
        }
        self.marked.contains(name)
    }

    /// `Tab`: toggle `n` items from the cursor, then move past them.
    pub(super) fn toggle_marks(&mut self, n: usize) {
        if !self.can_select() {
            return;
        }
        let Some(listing) = self.view_listing.clone() else {
            return;
        };
        for &i in self.view.iter().skip(self.cursor).take(n.max(1)) {
            let name = &listing.entries[i].name;
            if !self.marked.remove(name) {
                self.marked.insert(name.clone());
            }
        }
        let next = (self.cursor + n.max(1)).min(self.view.len().saturating_sub(1));
        self.set_cursor(next);
    }

    pub(super) fn start_visual(&mut self) {
        if !self.can_select() || self.view.is_empty() {
            return;
        }
        self.visual = match self.visual {
            Some(_) => None,
            None => Some(self.cursor),
        };
    }

    /// Only real files and folders can be selected (not drives, pins, machines).
    fn can_select(&mut self) -> bool {
        let ok = matches!(self.cwd, Loc::Fs(_) | Loc::Results { .. });
        if !ok {
            self.set_status("nothing to select here".into(), false);
        }
        ok
    }

    /// `Esc`: leave visual mode, else clear the selection. True when it did something.
    pub(super) fn clear_marks(&mut self) -> bool {
        if self.visual.take().is_some() {
            return true;
        }
        if self.marked.is_empty() {
            return false;
        }
        self.marked.clear();
        true
    }

    /// A key in visual mode that isn't a motion. True when handled.
    pub(super) fn visual_key(&mut self, ev: &KeyEvent) -> bool {
        if self.visual.is_none() || !self.keys_pending.is_empty() {
            return false;
        }
        if ev.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match ev.code {
            KeyCode::Char('y') => self.yank(Op::Copy, 1),
            KeyCode::Char('x') => self.yank(Op::Move, 1),
            KeyCode::Char('d') => self.start_delete(false, 1),
            KeyCode::Char('D') => self.start_delete(true, 1),
            KeyCode::Char('v' | 'V') | KeyCode::Esc => self.visual = None,
            KeyCode::Tab => {
                // Visual range into the selection, as Tab would.
                let names = self.names(&self.operand(1));
                self.visual = None;
                self.marked.extend(names);
            }
            _ => return false,
        }
        self.count = None;
        self.dirty = true;
        true
    }

    fn names(&self, idx: &[usize]) -> Vec<String> {
        let Some(listing) = &self.view_listing else {
            return Vec::new();
        };
        idx.iter()
            .map(|&i| listing.entries[i].name.clone())
            .collect()
    }

    // ---------------------------------------------------------- yank / paste

    /// `yy` / `x` (and `y` / `x` / `d` in visual mode).
    pub(super) fn yank(&mut self, op: Op, count: usize) {
        let idx = self.operand(count);
        let Some(listing) = self.view_listing.clone() else {
            return;
        };
        let mut items = Vec::new();
        for i in idx {
            let e = &listing.entries[i];
            if !matches!(e.kind, EntryKind::File | EntryKind::Dir) {
                continue;
            }
            let Ok(Loc::Fs(path)) = self.cwd.join(&e.name) else {
                continue;
            };
            items.push(Item {
                path,
                is_dir: e.kind == EntryKind::Dir,
                size: e.size,
            });
        }
        self.visual = None;
        if items.is_empty() {
            self.set_status("nothing to yank here".into(), false);
            return;
        }
        self.marked.clear();
        let n = items.len();
        let msg = match op {
            Op::Copy => format!("yanked {} (p pastes a copy)", items_word(n)),
            Op::Move => format!("cut {} (p moves them here)", items_word(n)),
        };
        self.register = Some(Register { op, items });
        self.set_status(msg, false);
    }

    /// `.`: the last copy-paste again, here.
    pub(super) fn repeat_paste(&mut self) {
        match self.last_paste.clone() {
            Some((reg, overwrite)) => {
                self.register = Some(reg);
                self.paste(overwrite);
            }
            None => self.set_status("nothing to repeat (a cut is pasted once)".into(), false),
        }
    }

    /// `p` / `P`.
    pub(super) fn paste(&mut self, overwrite: bool) {
        let Some(reg) = self.register.clone() else {
            self.set_status("nothing yanked (yy copies, x cuts)".into(), false);
            return;
        };
        let Some(dest) = self.cwd.as_fs().cloned() else {
            self.set_status("paste into a folder".into(), false);
            return;
        };
        let req = Request {
            op: reg.op,
            items: reg.items,
            dest,
            overwrite: if overwrite {
                Overwrite::Always
            } else {
                Overwrite::Never
            },
        };
        let native_max = self
            .config
            .jobs
            .native_max_bytes
            .unwrap_or(jobs::NATIVE_MAX_BYTES);
        let plan = match jobs::plan(&req, native_max) {
            Ok(p) => p,
            Err(e) => return self.set_status(e, true),
        };
        if reg.op == Op::Copy {
            self.last_paste = Some((
                Register {
                    op: reg.op,
                    items: req.items.clone(),
                },
                overwrite,
            ));
        }
        // Folders: is one an AVEVA project? The answer comes back as `AvevaChecked`.
        let dirs: Vec<String> = req
            .items
            .iter()
            .filter(|i| i.is_dir)
            .map(|i| i.path.display())
            .collect();
        if dirs.is_empty() {
            return self.confirm_paste(req, plan, Vec::new());
        }
        self.paste_token += 1;
        let token = self.paste_token;
        self.pending_paste = Some(PendingPaste { token, req, plan });
        self.set_status("checking the folders…".into(), false);
        self.worker
            .aveva_check(token, dirs, self.config.aveva_markers());
    }

    pub(super) fn on_aveva_checked(&mut self, token: u64, hits: Vec<String>) {
        let Some(p) = self.pending_paste.take() else {
            return;
        };
        if p.token != token {
            self.pending_paste = Some(p);
            return;
        }
        self.confirm_paste(p.req, p.plan, hits);
    }

    /// Asks when it replaces files (`P`) or touches an AVEVA project; else starts.
    fn confirm_paste(&mut self, req: Request, plan: Plan, aveva: Vec<String>) {
        let mut why = Vec::new();
        if let Some(prefix) = self.admin_prefix() {
            why.push(format!("{prefix}{} {} here.", req.op.word(), plan.title));
        }
        if !aveva.is_empty() {
            let names: Vec<String> = aveva
                .iter()
                .map(|d| d.rsplit('\\').next().unwrap_or(d).to_string())
                .collect();
            why.push(format!(
                "{} looks like an AVEVA project: make sure no one has it open.",
                names.join(", ")
            ));
        }
        if req.overwrite == Overwrite::Always {
            why.push(format!(
                "{} {}: files already in {} are REPLACED.",
                req.op.word(),
                plan.title,
                req.dest.name()
            ));
        }
        if why.is_empty() {
            return self.start_job(req, plan);
        }
        self.confirm = Some(Confirm::Paste {
            req: Box::new(req),
            plan: Box::new(plan),
            why: why.join(" "),
        });
        self.open_prompt_with(PromptKind::Confirm, "");
    }

    /// Starts the job and, for a cut, empties the register.
    pub(super) fn start_job(&mut self, req: Request, plan: Plan) {
        let id = self.new_job_id();
        let log =
            jobs::log_path(id, &plan.title).unwrap_or_else(|| format!("sentier-job-{id}.log"));
        let job = Job::new(
            id,
            &plan,
            req.op,
            req.dest.display(),
            req.overwrite,
            log.clone(),
        );
        let spec = Spec {
            plan: plan.clone(),
            moving: req.op == Op::Move,
            overwrite: req.overwrite,
            threads: self.config.jobs.robocopy_threads.unwrap_or(0),
            log,
            mirror: false,
            exclude_dirs: Vec::new(),
            exclude_files: Vec::new(),
            totals: None,
        };
        self.launch_job(job, spec);
        if req.op == Op::Move {
            self.register = None;
        }
        self.set_status(
            format!(
                "{} {} ({}) · J shows jobs",
                req.op.word(),
                plan.title,
                plan.backend.word()
            ),
            false,
        );
    }

    /// Hands the job to the worker and lists it first in the panel.
    pub(super) fn launch_job(&mut self, job: Job, spec: Spec) {
        let cancel = self.worker.start_job(job.id, spec);
        self.push_job(job, cancel);
    }

    /// Lists a job the worker already runs.
    pub(super) fn push_job(&mut self, job: Job, cancel: crate::jobs::Control) {
        self.job_cancel.insert(job.id, cancel);
        self.jobs.push(job);
        self.jobs_cursor = 0;
    }

    /// Ids are the start time in ms, so reports from earlier sessions never collide.
    pub(super) fn new_job_id(&mut self) -> u64 {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let id = now.max(self.last_job_id + 1);
        self.last_job_id = id;
        id
    }

    // -------------------------------------------------------------- job events

    pub(super) fn on_job_event(&mut self, id: u64, ev: JobEvent) {
        let now = Instant::now();
        let finished = matches!(ev, JobEvent::Finished { .. });
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        job.apply(ev, now);
        self.dirty = true;
        if !finished {
            return;
        }
        self.job_cancel.remove(&id);
        let report = job.report(now);
        let (phase, title) = (job.phase, job.title.clone());
        self.worker.save_report(report.clone());
        if let Some(p) = &report.preset {
            self.record_preset_run(&p.key, jobs::preset::LastRun::from_report(&report));
        }
        // The folders it changed show the change.
        let mut touched: Vec<String> = vec![report.dest.clone()];
        if report.op == Op::Move {
            touched.extend(report.steps.iter().map(|s| s.src.clone()));
        }
        self.refresh_touched(&touched);
        let msg = match phase {
            Phase::Done => format!("done: {title} · {}", done_counts(&report)),
            Phase::Cancelled => format!("cancelled: {title}"),
            _ => format!("FAILED: {title} · J for the report"),
        };
        self.set_status(msg, phase == Phase::Failed);
    }

    /// Re-lists the cwd and its preview if a job changed them.
    fn refresh_touched(&mut self, dirs: &[String]) {
        let shown: Vec<Loc> = [Some(self.cwd.clone()), self.preview_loc()]
            .into_iter()
            .flatten()
            .collect();
        for loc in shown {
            let Some(p) = loc.as_fs().map(|p| p.display()) else {
                continue;
            };
            if dirs.iter().any(|d| {
                d.eq_ignore_ascii_case(&p)
                    || jobs::parent_of(d).is_some_and(|x| x.eq_ignore_ascii_case(&p))
            }) {
                self.request(loc, true);
            }
        }
    }

    pub(super) fn on_job_reports(&mut self, reports: Vec<Report>) {
        let live: Vec<u64> = self.jobs.iter().map(|j| j.id).collect();
        self.old_reports = reports
            .into_iter()
            .filter(|r| !live.contains(&r.id))
            .collect();
        self.old_reports.sort_by_key(|r| std::cmp::Reverse(r.id));
        self.dirty = true;
    }

    /// From `tick`: phases (Finishing shows without new output), and the panel opens by
    /// itself for a job that runs longer than half a second.
    pub(super) fn jobs_tick(&mut self, now: Instant) {
        let mut open = false;
        for j in self.jobs.iter_mut().filter(|j| !j.phase.is_over()) {
            // Clocks move without events (a retry countdown, a stall, a pause).
            self.dirty = true;
            j.refresh(now);
            if now.duration_since(j.started) >= jobs::AUTO_PANEL_AFTER
                && !self.jobs_auto_opened.contains(&j.id)
            {
                self.jobs_auto_opened.insert(j.id);
                open = true;
            }
        }
        if open && self.mode == Mode::Normal && self.keys_pending.is_empty() {
            self.mode = Mode::Jobs;
            self.jobs_cursor = 0;
        }
    }

    pub fn jobs_running(&self) -> usize {
        self.jobs.iter().filter(|j| !j.phase.is_over()).count()
    }

    // ---------------------------------------------------------------- the panel

    /// Newest first: this session's jobs, then the kept reports.
    /// The panel's rows in screen order: running jobs (newest first), then finished ones
    /// (this session's, newest first, then kept reports).
    pub fn job_rows(&self) -> Vec<JobRow> {
        let live = (0..self.jobs.len()).rev();
        let mut rows: Vec<JobRow> = live
            .clone()
            .filter(|&i| !self.jobs[i].phase.is_over())
            .map(JobRow::Live)
            .collect();
        rows.extend(
            live.filter(|&i| self.jobs[i].phase.is_over())
                .map(JobRow::Live),
        );
        rows.extend((0..self.old_reports.len()).map(JobRow::Old));
        rows
    }

    pub fn selected_job_row(&self) -> Option<JobRow> {
        let rows = self.job_rows();
        rows.get(self.jobs_cursor.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// The report of a row: a finished job's, or a kept one.
    pub fn row_report(&self, row: JobRow) -> Option<Report> {
        match row {
            JobRow::Live(i) => {
                let j = self.jobs.get(i)?;
                j.phase.is_over().then(|| j.report(Instant::now()))
            }
            JobRow::Old(i) => self.old_reports.get(i).cloned(),
        }
    }

    pub(super) fn toggle_jobs_panel(&mut self) {
        self.mode = if self.mode == Mode::Jobs {
            Mode::Normal
        } else {
            self.jobs_cursor = 0;
            Mode::Jobs
        };
    }

    pub(super) fn on_jobs_key(&mut self, ev: KeyEvent) {
        let pending_d = std::mem::take(&mut self.jobs_pending_d);
        let n = self.job_rows().len();
        match ev.code {
            KeyCode::Char('j') | KeyCode::Down if n > 0 => {
                self.jobs_cursor = (self.jobs_cursor + 1).min(n - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.jobs_cursor = self.jobs_cursor.saturating_sub(1)
            }
            KeyCode::Char('g') => self.jobs_cursor = 0,
            KeyCode::Char('G') => self.jobs_cursor = n.saturating_sub(1),
            KeyCode::Char('c') => self.cancel_job(),
            KeyCode::Char('p') => self.pause_job(),
            KeyCode::Char('s') => self.skip_locked_file(),
            KeyCode::Char('R') => self.retry_locked_now(),
            KeyCode::Char('r') => self.retry_job(),
            KeyCode::Char('o') => self.open_job_log(),
            KeyCode::Char('d') if pending_d => self.dismiss_job(),
            KeyCode::Char('d') => self.jobs_pending_d = true,
            KeyCode::Enter => self.dismiss_job(),
            KeyCode::Char('J' | 'q') | KeyCode::Esc => self.mode = Mode::Normal,
            _ => {}
        }
    }

    fn cancel_job(&mut self) {
        let Some(JobRow::Live(i)) = self.selected_job_row() else {
            return self.set_status("only a running job can be cancelled".into(), false);
        };
        let Some(job) = self.jobs.get(i) else { return };
        match self.job_cancel.get(&job.id) {
            Some(c) if !job.phase.is_over() => {
                c.cancel();
                self.set_status(format!("cancelling {}…", job.title), false);
            }
            _ => self.set_status(format!("{} isn't running", job.title), false),
        }
    }

    /// The selected job if it's running, with its control.
    fn running_selected(&self) -> Option<(&Job, &crate::jobs::Control)> {
        let Some(JobRow::Live(i)) = self.selected_job_row() else {
            return None;
        };
        let job = self.jobs.get(i).filter(|j| !j.phase.is_over())?;
        Some((job, self.job_cancel.get(&job.id)?))
    }

    /// `p`: pause (robocopy stops; its locks go) or resume (it carries on where it was).
    fn pause_job(&mut self) {
        let Some((job, ctl)) = self.running_selected() else {
            return self.set_status("p pauses a running job".into(), false);
        };
        let on = !ctl.paused();
        ctl.set_paused(on);
        let msg = if on {
            format!("pausing {}… (p resumes)", job.title)
        } else {
            format!("resuming {}…", job.title)
        };
        self.set_status(msg, false);
    }

    /// `s`: leave out the locked file the job is waiting on.
    fn skip_locked_file(&mut self) {
        let Some((job, ctl)) = self.running_selected() else {
            return self.set_status("s skips a locked file a running job waits on".into(), false);
        };
        let Some(w) = job.waiting.as_ref() else {
            return self.set_status(
                "no locked file to skip: it isn't waiting on one".into(),
                false,
            );
        };
        ctl.skip();
        let name = w.path.rsplit('\\').next().unwrap_or(&w.path).to_string();
        self.set_status(format!("skipping {name}…"), false);
    }

    /// `R`: try the locked file again now instead of waiting.
    fn retry_locked_now(&mut self) {
        let Some((job, ctl)) = self.running_selected() else {
            return self.set_status(
                "R retries a locked file a running job waits on".into(),
                false,
            );
        };
        let Some(w) = job.waiting.as_ref() else {
            return self.set_status(
                "nothing to retry now: it isn't waiting on a file".into(),
                false,
            );
        };
        ctl.retry_now();
        let name = w.path.rsplit('\\').next().unwrap_or(&w.path).to_string();
        self.set_status(format!("trying {name} again…"), false);
    }

    /// `r`: a new job that copies only the failed files again.
    fn retry_job(&mut self) {
        let Some(report) = self.selected_job_row().and_then(|r| self.row_report(r)) else {
            return self.set_status("r retries a finished job's failed files".into(), false);
        };
        if report.failed.is_empty() {
            return self.set_status("no failed files to retry".into(), false);
        }
        let failed: Vec<String> = report.failed.iter().map(|f| f.path.clone()).collect();
        let steps = if report.steps.is_empty() {
            // Native / rename jobs put every file straight into the destination.
            jobs::flat_steps(&failed, &report.dest)
        } else {
            jobs::retry_steps(&report.steps, &failed)
        };
        if steps.is_empty() {
            return self.set_status("couldn't work out where the failed files go".into(), true);
        }
        let plan = Plan {
            backend: Backend::Robocopy,
            steps,
            pairs: Vec::new(),
            title: format!("retry: {}", report.title),
        };
        let Ok(Loc::Fs(dest)) = Loc::parse(&report.dest) else {
            return self.set_status(format!("bad destination {}", report.dest), true);
        };
        let req = Request {
            op: report.op,
            items: Vec::new(),
            dest,
            overwrite: Overwrite::Changed,
        };
        self.start_job(req, plan);
    }

    fn open_job_log(&mut self) {
        let log = match self.selected_job_row() {
            Some(JobRow::Live(i)) => self.jobs.get(i).map(|j| j.log.clone()),
            Some(JobRow::Old(i)) => self.old_reports.get(i).map(|r| r.log.clone()),
            None => None,
        };
        match log.as_deref().map(Loc::parse) {
            Some(Ok(loc)) => self.worker.open(loc),
            Some(Err(e)) => self.set_status(e.to_string(), true),
            None => {}
        }
    }

    /// `Enter` / `dd` on a finished job: forget it (its log stays).
    fn dismiss_job(&mut self) {
        let Some(row) = self.selected_job_row() else {
            return;
        };
        let id = match row {
            JobRow::Live(i) => match self.jobs.get(i) {
                Some(j) if j.phase.is_over() => {
                    let id = j.id;
                    self.jobs.remove(i);
                    id
                }
                Some(j) => {
                    return self.set_status(format!("{} is running: c cancels it", j.title), false);
                }
                None => return,
            },
            JobRow::Old(i) => self.old_reports.remove(i).id,
        };
        self.worker.delete_report(id);
        let n = self.job_rows().len();
        self.jobs_cursor = self.jobs_cursor.min(n.saturating_sub(1));
    }
}

/// `3 copied · 1 skipped · 3.80 GB`
pub fn done_counts(r: &Report) -> String {
    let mut parts = vec![format!("{} copied", r.copied)];
    if r.skipped > 0 {
        parts.push(format!("{} skipped (already there)", r.skipped));
    }
    if r.bytes > 0 {
        parts.push(jobs::human_bytes(r.bytes));
    }
    parts.join(" · ")
}
