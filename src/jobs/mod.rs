//! Copy / move jobs (CLAUDE.md section 4): the plan for a paste, the job's state and
//! phase, and its final report. Pure: the worker runs the plan and sends back the events
//! (robocopy's, parsed, or the same events made by the native backend), and the app
//! applies them here.

pub mod archive;
pub mod headless;
pub mod preset;
pub mod robocopy;
pub mod run;
pub mod zip;

use crate::fs::path::{FsPath, Loc};
use robocopy::{Event, Overwrite, Step, Tracker};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

/// Files at or above this go through robocopy (restartable, `/Z`).
pub const NATIVE_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Jobs running longer than this open the panel by themselves: long enough that a copy
/// done in a blink doesn't flash it, short enough to feel immediate.
pub const AUTO_PANEL_AFTER: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Copy,
    Move,
}

impl Op {
    pub fn word(self) -> &'static str {
        match self {
            Op::Copy => "copy",
            Op::Move => "move",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Robocopy,
    /// Small files, copied one by one.
    Native,
    /// A move within one volume: a rename, instant whatever the size.
    Rename,
}

impl Backend {
    pub fn word(self) -> &'static str {
        match self {
            Backend::Robocopy => "robocopy",
            Backend::Native => "native",
            Backend::Rename => "rename",
        }
    }
}

/// One selected item.
#[derive(Debug, Clone)]
pub struct Item {
    pub path: FsPath,
    pub is_dir: bool,
    pub size: u64,
}

/// What `p` / `P` asked for.
#[derive(Debug, Clone)]
pub struct Request {
    pub op: Op,
    pub items: Vec<Item>,
    pub dest: FsPath,
    pub overwrite: Overwrite,
}

/// A source and destination path pair, for the native and rename backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub src: String,
    pub dst: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub backend: Backend,
    /// robocopy runs, in order.
    pub steps: Vec<Step>,
    /// native / rename work.
    pub pairs: Vec<Pair>,
    /// `3 files → hitmac`, `HIT → local`.
    pub title: String,
}

fn same_volume(a: &FsPath, b: &FsPath) -> bool {
    a.volume_root()
        .display()
        .eq_ignore_ascii_case(&b.volume_root().display())
}

fn parent(p: &FsPath) -> Option<FsPath> {
    Loc::Fs(p.clone()).parent()?.as_fs().cloned()
}

/// True when `it` sits right in `dest` (a paste there makes a copy beside it).
pub fn in_folder(it: &Item, dest: &FsPath) -> bool {
    parent(&it.path).is_some_and(|p| p.display().eq_ignore_ascii_case(&dest.display()))
}

/// `report.pdf` → `report_1.pdf`, then `_2`…: the first name not in `taken` (lowercased).
/// Yazi's naming for a copy pasted where its original is. A folder keeps its dots.
pub fn free_name(name: &str, is_dir: bool, taken: &std::collections::HashSet<String>) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => name.split_at(i),
        _ => (name, ""),
    };
    (1u32..)
        .map(|n| format!("{stem}_{n}{ext}"))
        .find(|c| !taken.contains(&c.to_lowercase()))
        .unwrap_or_else(|| format!("{stem}_copy{ext}"))
}

/// Works out how to paste: refuses a folder into itself. A copy pasted where its original
/// is gets a free name (`report_1.pdf`, see [`free_name`]); `taken` is what the
/// destination already holds. Such files go through the native backend whatever their
/// size (robocopy can't rename a file), so the caller pastes copied-in-place files and
/// folders as separate jobs.
pub fn plan(req: &Request, native_max: u64, taken: &[String]) -> Result<Plan, String> {
    if req.items.is_empty() {
        return Err("nothing to paste".into());
    }
    for it in &req.items {
        if it.is_dir && req.dest.starts_with(&it.path) {
            return Err(format!(
                "can't {} {} into itself",
                req.op.word(),
                it.path.name()
            ));
        }
        if req.op == Op::Move && in_folder(it, &req.dest) {
            return Err(format!("{} is already in this folder", it.path.name()));
        }
    }
    let dup_files = req
        .items
        .iter()
        .any(|i| !i.is_dir && in_folder(i, &req.dest));
    if dup_files && req.items.iter().any(|i| i.is_dir) {
        return Err("copy the folders and the files here separately".into());
    }
    // The names things get in `dest`: their own, or a free one beside the original.
    let mut used: std::collections::HashSet<String> =
        taken.iter().map(|t| t.to_lowercase()).collect();
    let names: Vec<String> = req
        .items
        .iter()
        .map(|it| {
            let name = it.path.name();
            if !in_folder(it, &req.dest) {
                return name;
            }
            let free = free_name(&name, it.is_dir, &used);
            used.insert(free.to_lowercase());
            free
        })
        .collect();
    let title = match (&req.items[..], &names[..]) {
        ([one], [new]) if in_folder(one, &req.dest) => format!("{} → {new}", one.path.name()),
        _ => title(&req.items, &req.dest),
    };
    let pair = |i: usize| -> Result<Pair, String> {
        let it = &req.items[i];
        Ok(Pair {
            src: it.path.display(),
            dst: req
                .dest
                .join(&names[i])
                .map_err(|e| e.to_string())?
                .display(),
            size: it.size,
            is_dir: it.is_dir,
        })
    };
    let pairs = || (0..req.items.len()).map(pair).collect::<Result<_, _>>();
    if req.op == Op::Move && req.items.iter().all(|i| same_volume(&i.path, &req.dest)) {
        return Ok(Plan {
            backend: Backend::Rename,
            steps: Vec::new(),
            pairs: pairs()?,
            title,
        });
    }
    if dup_files || req.items.iter().all(|i| !i.is_dir && i.size < native_max) {
        return Ok(Plan {
            backend: Backend::Native,
            steps: Vec::new(),
            pairs: pairs()?,
            title,
        });
    }
    // robocopy: one run per folder, and one per source folder for the files.
    let mut steps: Vec<Step> = Vec::new();
    for (i, it) in req.items.iter().enumerate().filter(|(_, i)| i.is_dir) {
        steps.push(Step {
            src: it.path.display(),
            dst: pair(i)?.dst,
            files: Vec::new(),
        });
    }
    for it in req.items.iter().filter(|i| !i.is_dir) {
        let Some(dir) = parent(&it.path) else {
            continue;
        };
        let src = dir.display();
        match steps
            .iter_mut()
            .find(|s| !s.files.is_empty() && s.src.eq_ignore_ascii_case(&src))
        {
            Some(s) => s.files.push(it.path.name()),
            None => steps.push(Step {
                src,
                dst: req.dest.display(),
                files: vec![it.path.name()],
            }),
        }
    }
    Ok(Plan {
        backend: Backend::Robocopy,
        steps,
        pairs: Vec::new(),
        title,
    })
}

/// `HIT → local` for one item, `3 files → hitmac`, `2 folders, 1 file → hitmac`.
fn title(items: &[Item], dest: &FsPath) -> String {
    let to = dest.name();
    if let [one] = items {
        return format!("{} → {to}", one.path.name());
    }
    let dirs = items.iter().filter(|i| i.is_dir).count();
    let files = items.len() - dirs;
    let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
    let what = match (dirs, files) {
        (0, f) => n(f, "file", "files"),
        (d, 0) => n(d, "folder", "folders"),
        (d, f) => format!("{}, {}", n(d, "folder", "folders"), n(f, "file", "files")),
    };
    format!("{what} → {to}")
}

/// The robocopy steps that copy only `failed` (source paths) again.
pub fn retry_steps(steps: &[Step], failed: &[String]) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    for f in failed {
        // The step it came from: the longest source folder that contains it.
        let Some(step) = steps
            .iter()
            .filter(|s| {
                f.len() > s.src.len()
                    && f[..s.src.len()].eq_ignore_ascii_case(&s.src)
                    && f[s.src.len()..].starts_with('\\')
                    || s.src.ends_with('\\') && f.to_lowercase().starts_with(&s.src.to_lowercase())
            })
            .max_by_key(|s| s.src.len())
        else {
            continue;
        };
        let rel = f[step.src.len()..].trim_start_matches('\\');
        let (dir, name) = match rel.rfind('\\') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => ("", rel),
        };
        let join = |base: &str| {
            if dir.is_empty() {
                base.to_string()
            } else {
                format!("{}\\{dir}", base.trim_end_matches('\\'))
            }
        };
        let (src, dst) = (join(&step.src), join(&step.dst));
        match out.iter_mut().find(|s| s.src.eq_ignore_ascii_case(&src)) {
            Some(s) => s.files.push(name.to_string()),
            None => out.push(Step {
                src,
                dst,
                files: vec![name.to_string()],
            }),
        }
    }
    out
}

// ------------------------------------------------------------------ the job

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Scanning,
    Copying,
    Retrying,
    /// Every byte is written but robocopy hasn't exited: the old "stuck at 99 %".
    Finishing,
    /// `p` in the jobs panel: robocopy is stopped until resumed (it restarts with `/Z`).
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl Phase {
    pub fn word(self) -> &'static str {
        match self {
            Phase::Scanning => "SCANNING",
            Phase::Copying => "COPYING",
            Phase::Retrying => "RETRYING",
            Phase::Finishing => "FINISHING",
            Phase::Paused => "PAUSED",
            Phase::Done => "DONE",
            Phase::Failed => "FAILED",
            Phase::Cancelled => "CANCELLED",
        }
    }

    pub fn is_over(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Cancelled)
    }
}

/// What the worker sends about a running job.
#[derive(Debug, Clone)]
pub enum JobEvent {
    /// The `/L` pass (or the selection's sizes) found this much to copy.
    Scanned { files: u64, bytes: u64 },
    /// robocopy's (or the native backend's) output, parsed.
    Output(Vec<Event>),
    /// One robocopy run ended with this exit code.
    StepDone(i32),
    /// The job could not run at all (robocopy missing, refused…).
    Error(String),
    /// Everything ended (or was cancelled).
    Finished { cancelled: bool },
    /// A backup moved on to this step.
    Stage(zip::BackupStep),
    /// Bytes through the current backup step, and the file it's on.
    StageProgress {
        done: u64,
        total: u64,
        current: Option<String>,
    },
    /// The backup zip is written, verified and has its final name.
    Produced {
        path: String,
        bytes: u64,
        entries: u64,
    },
    /// Old zips deleted by retention.
    Pruned(Vec<String>),
    /// Paused (true) or running again (false).
    Paused(bool),
    /// A locked file sentier will try again: attempt `attempt` of `max` at `next`.
    Waiting {
        path: String,
        code: u32,
        message: String,
        attempt: u32,
        max: u32,
        next: Instant,
    },
    /// A file that failed earlier was copied on retry `attempts`.
    Recovered { path: String, attempts: u32 },
    /// `s`: a locked file left out on purpose.
    Skipped(String),
    /// A locked file still locked after every retry (it stays failed).
    GaveUp { path: String, attempts: u32 },
}

/// What the panel can tell a running job: cancel, pause / resume, skip the locked file it
/// waits on, retry that file now. Atomic flags the runner polls, so the UI never waits.
#[derive(Debug, Clone, Default)]
pub struct Control {
    pub cancel: crate::fs::worker::CancelToken,
    flags: std::sync::Arc<Flags>,
}

#[derive(Debug, Default)]
struct Flags {
    paused: std::sync::atomic::AtomicBool,
    skip: std::sync::atomic::AtomicBool,
    retry_now: std::sync::atomic::AtomicBool,
}

impl Control {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn paused(&self) -> bool {
        self.flags.paused.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_paused(&self, on: bool) {
        self.flags
            .paused
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// `s`: skip the locked file being waited on.
    pub fn skip(&self) {
        self.flags
            .skip
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// `R`: try the locked file again now.
    pub fn retry_now(&self) {
        self.flags
            .retry_now
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn take_skip(&self) -> bool {
        self.flags
            .skip
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn take_retry_now(&self) -> bool {
        self.flags
            .retry_now
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }
}

/// A locked file sentier is waiting to try again (the stall banner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub path: String,
    pub code: u32,
    pub message: String,
    pub attempt: u32,
    pub max: u32,
    pub next: Instant,
}

/// No bytes for this long while copying: the panel says why (the stall banner).
pub const STALL_AFTER: Duration = Duration::from_secs(10);

/// Lines kept for the panel's ACTIVITY, and seconds of speed history for its chart.
pub const ACTIVITY_KEEP: usize = 50;
pub const HISTORY: usize = 64;

/// One line of the panel's ACTIVITY (also kept in the report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub at: SystemTime,
    pub kind: ActKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActKind {
    Scan,
    Start,
    Copy,
    /// A retry, with its attempt number.
    Retry(u32),
    Error,
    Skip,
    Done,
}

impl ActKind {
    pub fn word(self) -> String {
        match self {
            ActKind::Scan => "scan".into(),
            ActKind::Start => "start".into(),
            ActKind::Copy => "copy".into(),
            ActKind::Retry(n) => format!("retry {n}"),
            ActKind::Error => "error".into(),
            ActKind::Skip => "skip".into(),
            ActKind::Done => "done".into(),
        }
    }
}

/// `1 file`, `3 files`.
pub fn files_word(n: u64) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

fn file_name(path: &str) -> &str {
    path.trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(path)
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub title: String,
    pub op: Op,
    pub backend: Backend,
    pub dest: String,
    pub steps: Vec<Step>,
    pub overwrite: Overwrite,
    pub total_files: Option<u64>,
    pub total_bytes: Option<u64>,
    pub tracker: Tracker,
    pub started: Instant,
    pub started_at: SystemTime,
    pub phase: Phase,
    pub phase_since: Instant,
    /// The OR of every step's exit code (robocopy's bitmask).
    pub exit: Option<i32>,
    pub error: Option<String>,
    pub log: String,
    pub ended: Option<Instant>,
    samples: VecDeque<(Instant, u64)>,
    /// The last non-zero speed (bytes/s), shown while the current one is unknown.
    pub last_speed: Option<f64>,
    /// Started from a sync / backup preset: its last-run state is updated at the end.
    pub preset: Option<preset::PresetRef>,
    /// A backup's steps (staging uses the robocopy tracker like any copy).
    pub backup: Option<zip::BackupState>,
    /// When each phase began, in order (the panel's stepper shows how long each took).
    pub stage_log: Vec<(Phase, Instant)>,
    /// Bytes per second, one sample per second, newest last (the THROUGHPUT chart).
    pub history: VecDeque<f64>,
    hist_at: Option<Instant>,
    hist_bytes: u64,
    /// The speed smoothed over ~10 s (the ETA uses it, so it doesn't jump around).
    pub smooth: Option<f64>,
    /// When bytes last moved (stall detection).
    pub last_progress_at: Instant,
    last_bytes: u64,
    /// What happened, newest last (ACTIVITY).
    pub activity: VecDeque<Activity>,
    /// Paused with `p` (the runner confirmed it).
    pub paused: bool,
    /// The locked file sentier waits to try again, if any.
    pub waiting: Option<Waiting>,
    /// Attempts sentier made on locked files.
    pub retry_attempts: u32,
    /// Files left out with `s`.
    pub skipped_files: Vec<String>,
}

impl Job {
    pub fn new(
        id: u64,
        plan: &Plan,
        op: Op,
        dest: String,
        overwrite: Overwrite,
        log: String,
    ) -> Job {
        let now = Instant::now();
        Job {
            id,
            title: plan.title.clone(),
            op,
            backend: plan.backend,
            dest,
            steps: plan.steps.clone(),
            overwrite,
            total_files: None,
            total_bytes: None,
            tracker: Tracker::new(false),
            started: now,
            started_at: SystemTime::now(),
            phase: Phase::Scanning,
            phase_since: now,
            exit: None,
            error: None,
            log,
            ended: None,
            samples: VecDeque::new(),
            last_speed: None,
            preset: None,
            backup: None,
            stage_log: vec![(Phase::Scanning, now)],
            history: VecDeque::new(),
            hist_at: None,
            hist_bytes: 0,
            smooth: None,
            last_progress_at: now,
            last_bytes: 0,
            activity: VecDeque::new(),
            paused: false,
            waiting: None,
            retry_attempts: 0,
            skipped_files: Vec::new(),
        }
    }

    /// Adds a line to ACTIVITY.
    pub fn note(&mut self, kind: ActKind, text: String) {
        self.activity.push_back(Activity {
            at: SystemTime::now(),
            kind,
            text,
        });
        while self.activity.len() > ACTIVITY_KEEP {
            self.activity.pop_front();
        }
    }

    /// One robocopy / native event, with its ACTIVITY line.
    fn take_event(&mut self, e: Event) {
        let failed_before = self.tracker.failed.len();
        let note = match &e {
            Event::File { class, size, path }
                if !matches!(
                    class,
                    robocopy::FileClass::Same | robocopy::FileClass::Extra
                ) =>
            {
                Some((
                    ActKind::Copy,
                    format!("{} ({})", file_name(path), human_bytes(*size)),
                ))
            }
            _ => None,
        };
        let err = match &e {
            Event::Error(r) => Some(r.clone()),
            _ => None,
        };
        self.tracker.apply(e);
        if let Some((k, t)) = note {
            self.note(k, t);
        }
        if let Some(r) = err {
            let what = format!(
                "{}: {} (error {})",
                r.path.as_deref().map_or(r.action.as_str(), file_name),
                r.message,
                r.code
            );
            let retry = r.path.as_ref().and_then(|p| {
                self.tracker
                    .retries
                    .iter()
                    .rev()
                    .find(|x| &x.path == p && x.ok.is_none())
                    .map(|x| x.attempts)
            });
            let kind = match retry {
                Some(n) if self.tracker.failed.len() == failed_before => ActKind::Retry(n),
                _ => ActKind::Error,
            };
            self.note(kind, what);
        }
    }

    /// From the ticks: one speed sample per second once copying started, the smoothed
    /// speed, and when bytes last moved.
    fn tick_history(&mut self, now: Instant) {
        let bytes = self.progress_bytes();
        if bytes > self.last_bytes {
            self.last_progress_at = now;
        }
        self.last_bytes = bytes;
        if self.phase == Phase::Scanning {
            return;
        }
        let Some(at) = self.hist_at else {
            self.hist_at = Some(now);
            self.hist_bytes = bytes;
            return;
        };
        let dt = now.duration_since(at).as_secs_f64();
        if dt < 1.0 {
            return;
        }
        let rate = bytes.saturating_sub(self.hist_bytes) as f64 / dt;
        // A gap (a slow tick) fills each missing second with the average over it.
        for _ in 0..(dt.floor() as usize).clamp(1, HISTORY) {
            self.history.push_back(rate);
            // ~10 s smoothing: each second weighs about a tenth.
            self.smooth = Some(match self.smooth {
                Some(s) => s + 0.1 * (rate - s),
                None => rate,
            });
        }
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        self.hist_at = Some(now);
        self.hist_bytes = bytes;
    }

    /// No bytes for [`STALL_AFTER`] while copying or finishing, or waiting on a locked
    /// file: the panel shows the stall banner. Never while paused or scanning.
    pub fn stalled(&self, now: Instant) -> bool {
        if self.paused
            || matches!(self.phase, Phase::Scanning | Phase::Paused)
            || self.phase.is_over()
        {
            return false;
        }
        self.waiting.is_some() || now.duration_since(self.last_progress_at) > STALL_AFTER
    }

    /// Where it copies from: the first step's source (the file itself for one file).
    pub fn source(&self) -> String {
        match self.steps.first() {
            Some(s) if s.files.len() == 1 => {
                format!("{}\\{}", s.src.trim_end_matches('\\'), s.files[0])
            }
            Some(s) => s.src.clone(),
            None => String::new(),
        }
    }

    /// Average speed since copying started (bytes/s).
    pub fn avg_speed(&self, now: Instant) -> Option<f64> {
        let start = self
            .stage_log
            .iter()
            .find(|(p, _)| *p != Phase::Scanning)
            .map(|(_, t)| *t)?;
        let secs = self
            .ended
            .unwrap_or(now)
            .duration_since(start)
            .as_secs_f64();
        (secs >= 1.0).then(|| self.progress_bytes() as f64 / secs)
    }

    /// How long each stage took (Retrying counts as Copying), in order; the last one
    /// still running until the job ends.
    pub fn stage_times(&self, now: Instant) -> Vec<(Phase, Duration)> {
        stage_times(&self.stage_log, self.ended.unwrap_or(now))
    }

    /// The backup step past staging, if any: progress then comes from that step.
    fn later_step(&self) -> Option<&zip::BackupState> {
        self.backup
            .as_ref()
            .filter(|b| b.step.is_some_and(|s| s != zip::BackupStep::Staging))
    }

    /// Bytes done in what's running now (robocopy's, or the backup step's).
    fn progress_bytes(&self) -> u64 {
        match self.later_step() {
            Some(b) => b.done,
            None => self.tracker.bytes_now(),
        }
    }

    fn sample(&mut self, now: Instant) {
        self.samples.push_back((now, self.progress_bytes()));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(3))
        {
            self.samples.pop_front();
        }
    }

    pub fn apply(&mut self, ev: JobEvent, now: Instant) {
        match ev {
            JobEvent::Scanned { files, bytes } => {
                self.total_files = Some(files);
                self.total_bytes = Some(bytes);
                self.note(
                    ActKind::Scan,
                    format!("{} to copy, {}", files_word(files), human_bytes(bytes)),
                );
            }
            JobEvent::Output(events) => {
                for e in events {
                    self.take_event(e);
                }
                self.sample(now);
            }
            JobEvent::Stage(step) => {
                let b = self.backup.get_or_insert_with(Default::default);
                b.step = Some(step);
                (b.done, b.total, b.current) = (0, 0, None);
                self.samples.clear();
                self.last_speed = None;
                self.hist_bytes = 0;
                self.last_bytes = 0;
                self.note(ActKind::Start, step.word().to_string());
            }
            JobEvent::StageProgress {
                done,
                total,
                current,
            } => {
                let b = self.backup.get_or_insert_with(Default::default);
                (b.done, b.total, b.current) = (done, total, current);
                self.sample(now);
            }
            JobEvent::Produced {
                path,
                bytes,
                entries,
            } => {
                let b = self.backup.get_or_insert_with(Default::default);
                b.outcome = Some(zip::BackupOutcome {
                    zip: path,
                    bytes,
                    entries,
                    pruned: Vec::new(),
                    extracted: b.kind == zip::StepSet::Extract,
                });
            }
            JobEvent::Pruned(names) => {
                let b = self.backup.get_or_insert_with(Default::default);
                if let Some(o) = b.outcome.as_mut() {
                    o.pruned = names.clone();
                }
                b.pruned = names;
            }
            JobEvent::Paused(on) => {
                self.paused = on;
                self.note(
                    ActKind::Start,
                    if on {
                        "paused (p resumes)".into()
                    } else {
                        "resumed".into()
                    },
                );
            }
            JobEvent::Waiting {
                path,
                code,
                message,
                attempt,
                max,
                next,
            } => {
                let secs = next.saturating_duration_since(now).as_secs();
                self.note(
                    ActKind::Retry(attempt),
                    format!(
                        "{}: {} (error {code}); try {attempt} of {max} in {secs} s",
                        file_name(&path),
                        message.trim().trim_end_matches('.')
                    ),
                );
                self.retry_attempts += 1;
                self.waiting = Some(Waiting {
                    path,
                    code,
                    message,
                    attempt,
                    max,
                    next,
                });
            }
            JobEvent::Recovered { path, attempts } => {
                self.tracker.recovered(&path, attempts);
                self.waiting = None;
                self.note(
                    ActKind::Done,
                    format!("{} copied on retry {attempts}", file_name(&path)),
                );
            }
            JobEvent::Skipped(path) => {
                self.tracker.forget_failed(&path);
                self.waiting = None;
                self.note(ActKind::Skip, format!("{} left out (s)", file_name(&path)));
                self.skipped_files.push(path);
            }
            JobEvent::GaveUp { path, attempts } => {
                self.waiting = None;
                self.note(
                    ActKind::Error,
                    format!("{}: still locked after {attempts} tries", file_name(&path)),
                );
            }
            JobEvent::StepDone(code) => {
                self.tracker.finish();
                self.exit =
                    Some(self.exit.unwrap_or(0) | code.max(0) | if code < 0 { 16 } else { 0 });
            }
            JobEvent::Error(e) => self.error = Some(e),
            JobEvent::Finished { cancelled } => {
                self.tracker.finish();
                self.ended = Some(now);
                // robocopy runs with `/R:0` and sentier retries locked files itself, so its
                // "some copies failed" (8) can be out of date: what failed in the end is
                // what the tracker still lists. 16 (fatal) always counts.
                let failed = self.error.is_some()
                    || self.exit.is_some_and(|c| c >= 16)
                    || !self.tracker.failed.is_empty()
                    || (self.exit.is_some_and(|c| c & 8 != 0) && !self.tracker.errors.is_empty());
                if !failed && let Some(c) = self.exit.as_mut() {
                    *c &= !8;
                }
                self.paused = false;
                self.waiting = None;
                self.set_phase(
                    if cancelled {
                        Phase::Cancelled
                    } else if failed {
                        Phase::Failed
                    } else {
                        Phase::Done
                    },
                    now,
                );
                let took = clock(self.elapsed(now));
                let text = match self.phase {
                    Phase::Cancelled => format!("cancelled after {took}"),
                    Phase::Failed => match (&self.error, self.tracker.failed.len()) {
                        (Some(e), _) => format!("failed: {e}"),
                        (None, n) => format!(
                            "{} copied, {} failed, in {took}",
                            files_word(self.tracker.files_done),
                            n
                        ),
                    },
                    // A zip made or an archive extracted: what came out.
                    _ => match self.backup.as_ref().and_then(|b| b.outcome.as_ref()) {
                        Some(o) => format!(
                            "{} · {} entries · {} in {took}",
                            file_name(&o.zip),
                            o.entries,
                            human_bytes(o.bytes)
                        ),
                        None => format!(
                            "{} copied ({}) in {took}",
                            files_word(self.tracker.files_done),
                            human_bytes(self.tracker.bytes_done)
                        ),
                    },
                };
                let kind = if self.phase == Phase::Done {
                    ActKind::Done
                } else {
                    ActKind::Error
                };
                self.note(kind, text);
                return;
            }
        }
        self.refresh(now);
    }

    fn set_phase(&mut self, p: Phase, now: Instant) {
        if self.phase != p {
            self.phase = p;
            self.phase_since = now;
            self.stage_log.push((p, now));
        }
    }

    /// The phase from what is known (called on events and on ticks).
    pub fn refresh(&mut self, now: Instant) {
        if self.phase.is_over() {
            return;
        }
        let t = &self.tracker;
        let p = if self.paused {
            Phase::Paused
        } else if self.waiting.is_some() {
            Phase::Retrying
        } else if self.later_step().is_some() {
            Phase::Copying
        } else if self.total_files.is_none() {
            Phase::Scanning
        } else if t.retrying() {
            Phase::Retrying
        } else if self.all_written() {
            Phase::Finishing
        } else {
            Phase::Copying
        };
        self.set_phase(p, now);
        self.tick_history(now);
        if let Some(s) = self.speed(now)
            && s > 0.0
        {
            self.last_speed = Some(s);
        }
    }

    /// Every file and byte counted by the scan is done, and something was to do.
    fn all_written(&self) -> bool {
        let (Some(files), Some(bytes)) = (self.total_files, self.total_bytes) else {
            return false;
        };
        let t = &self.tracker;
        files > 0
            && t.files_done + t.failed.len() as u64 >= files
            && t.bytes_now() >= bytes
            && t.current_path().is_none()
    }

    /// Bytes per second over the last 3 s; None until there are two samples.
    pub fn speed(&self, now: Instant) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let b1 = self.progress_bytes();
        let dt = now.duration_since(t0).as_secs_f64();
        if dt < 0.5 {
            return None;
        }
        Some(b1.saturating_sub(b0) as f64 / dt)
    }

    /// 0.0–1.0 by bytes (files when there are no bytes).
    pub fn fraction(&self) -> f64 {
        if let Some(b) = self.later_step() {
            return if b.total > 0 {
                (b.done as f64 / b.total as f64).min(1.0)
            } else {
                0.0
            };
        }
        match (self.total_bytes, self.total_files) {
            (Some(b), _) if b > 0 => (self.tracker.bytes_now() as f64 / b as f64).min(1.0),
            (_, Some(f)) if f > 0 => (self.tracker.files_done as f64 / f as f64).min(1.0),
            (Some(_), Some(_)) => 1.0,
            _ => 0.0,
        }
    }

    pub fn eta(&self, now: Instant) -> Option<Duration> {
        let total = match self.later_step() {
            Some(b) => b.total,
            None => self.total_bytes?,
        };
        let speed = self
            .smooth
            .filter(|s| *s > 1.0)
            .or_else(|| self.speed(now).filter(|s| *s > 1.0))?;
        let left = total.saturating_sub(self.progress_bytes());
        Some(Duration::from_secs_f64(left as f64 / speed))
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.ended.unwrap_or(now).duration_since(self.started)
    }

    /// The final report (only meaningful once it's over).
    pub fn report(&self, now: Instant) -> Report {
        let t = &self.tracker;
        let s = t.summary;
        let skipped = s.map_or(0, |s| s.files.skipped);
        Report {
            id: self.id,
            title: self.title.clone(),
            op: self.op,
            backend: self.backend,
            dest: self.dest.clone(),
            started: self.started_at,
            seconds: self.elapsed(now).as_secs(),
            phase: self.phase,
            exit: self.exit,
            error: self.error.clone(),
            copied: t.files_done,
            bytes: t.bytes_done,
            skipped,
            failed: t
                .failed
                .iter()
                .map(|f| FailedFile {
                    path: f.path.clone(),
                    code: f.code,
                    message: f.message.clone(),
                })
                .collect(),
            extra: t.extras.len() as u64,
            retried: t
                .retries
                .iter()
                .filter(|r| r.ok == Some(true))
                .map(|r| (r.path.clone(), r.attempts))
                .collect(),
            errors: t
                .errors
                .iter()
                .map(|e| {
                    format!(
                        "ERROR {} {} {}: {}",
                        e.code,
                        e.action,
                        e.path.as_deref().unwrap_or(""),
                        e.message
                    )
                })
                .collect(),
            log: self.log.clone(),
            steps: self.steps.clone(),
            overwrite: self.overwrite,
            preset: self.preset.clone(),
            backup: self.backup.as_ref().and_then(|b| b.outcome.clone()),
            source: self.source(),
            stages: self
                .stage_times(now)
                .into_iter()
                .map(|(p, d)| (p, d.as_secs()))
                .collect(),
            history: self.history.iter().map(|v| v.round() as u64).collect(),
            activity: self.activity.iter().cloned().collect(),
            skipped_files: self.skipped_files.clone(),
        }
    }
}

/// How long each stage took, from when each phase began (Retrying counts as Copying,
/// the end states take no time), up to `end`.
pub fn stage_times(log: &[(Phase, Instant)], end: Instant) -> Vec<(Phase, Duration)> {
    let mut out: Vec<(Phase, Duration)> = Vec::new();
    for (i, (p, t)) in log.iter().enumerate() {
        if p.is_over() {
            continue;
        }
        let next = log.get(i + 1).map_or(end, |(_, t)| *t);
        let p = if *p == Phase::Retrying {
            Phase::Copying
        } else {
            *p
        };
        let d = next.saturating_duration_since(*t);
        match out.iter_mut().find(|(q, _)| *q == p) {
            Some((_, sum)) => *sum += d,
            None => out.push((p, d)),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedFile {
    pub path: String,
    pub code: u32,
    pub message: String,
}

/// What's kept after a job, in `%LOCALAPPDATA%\sentier\jobs\<id>.json`, until dismissed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub id: u64,
    pub title: String,
    pub op: Op,
    pub backend: Backend,
    pub dest: String,
    pub started: SystemTime,
    pub seconds: u64,
    pub phase: Phase,
    pub exit: Option<i32>,
    pub error: Option<String>,
    pub copied: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub failed: Vec<FailedFile>,
    pub extra: u64,
    /// Files that worked after retries, with the attempt that worked.
    pub retried: Vec<(String, u32)>,
    pub errors: Vec<String>,
    pub log: String,
    /// For `r` (retry failed only).
    pub steps: Vec<Step>,
    pub overwrite: Overwrite,
    #[serde(default)]
    pub preset: Option<preset::PresetRef>,
    /// A backup's zip, when it got that far.
    #[serde(default)]
    pub backup: Option<zip::BackupOutcome>,
    /// Where it came from (the first source).
    #[serde(default)]
    pub source: String,
    /// Seconds each stage took (the stepper).
    #[serde(default)]
    pub stages: Vec<(Phase, u64)>,
    /// Bytes per second, one per second, the last [`HISTORY`] (the chart).
    #[serde(default)]
    pub history: Vec<u64>,
    /// The last [`ACTIVITY_KEEP`] events.
    #[serde(default)]
    pub activity: Vec<Activity>,
    /// Files left out on purpose (`s` on a locked file).
    #[serde(default)]
    pub skipped_files: Vec<String>,
}

impl Report {
    /// `FAILED robocopy exit 9 = 1 (files copied) + 8 (some copies failed)`
    pub fn result_line(&self) -> String {
        if let (Phase::Done, Some(b)) = (self.phase, &self.backup) {
            return match b.extracted {
                true => format!("DONE extracted, every CRC checked ({} entries)", b.entries),
                false => format!("DONE zip made and verified ({} entries)", b.entries),
            };
        }
        let mut s = self.phase.word().to_string();
        if let Some(code) = self.exit
            && self.backend == Backend::Robocopy
        {
            s.push_str(&format!(
                " robocopy exit {code}  =  {}",
                robocopy::exit_meaning(code).parts
            ));
        }
        if let Some(e) = &self.error {
            s.push_str(&format!(": {e}"));
        }
        s
    }
}

/// `3.80 GB`, `512 KB`, `12 B`.
pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}

/// `0:47`, `2:38`, `1:02:03`.
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ------------------------------------------------------------------ files

/// `%LOCALAPPDATA%\sentier\jobs`: logs and kept reports.
pub fn jobs_dir() -> Option<std::path::PathBuf> {
    // Tests never write into the user's folder.
    if cfg!(test) {
        return Some(std::env::temp_dir().join("sentier-test-jobs"));
    }
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::PathBuf::from(base).join("sentier").join("jobs"))
}

/// `…\jobs\2026-11-05_1951_HIT-local.log` (the title's letters and digits only).
pub fn log_path(id: u64, title: &str) -> Option<String> {
    let stamp = chrono::DateTime::from_timestamp_millis(id as i64)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d_%H%M%S")
                .to_string()
        })
        .unwrap_or_else(|| id.to_string());
    Some(
        jobs_dir()?
            .join(format!("{stamp}_{}.log", slug(title)))
            .display()
            .to_string(),
    )
}

/// `HIT → local` → `HIT-local`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    out.chars().take(40).collect()
}

/// The folder part of `C:\a\b.txt` (`C:\a`).
pub fn parent_of(path: &str) -> Option<&str> {
    let p = path.trim_end_matches('\\');
    let i = p.rfind('\\')?;
    Some(if i == 2 && p.as_bytes().get(1) == Some(&b':') {
        &p[..3]
    } else {
        &p[..i]
    })
}

/// Retry steps for jobs that put every file straight into `dest` (native, rename).
pub fn flat_steps(failed: &[String], dest: &str) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    for f in failed {
        let (Some(dir), Some(name)) = (parent_of(f), f.rsplit('\\').next()) else {
            continue;
        };
        match out.iter_mut().find(|s| s.src.eq_ignore_ascii_case(dir)) {
            Some(s) => s.files.push(name.to_string()),
            None => out.push(Step {
                src: dir.to_string(),
                dst: dest.to_string(),
                files: vec![name.to_string()],
            }),
        }
    }
    out
}

pub fn save_report(dir: &std::path::Path, r: &Report) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{}.json.tmp", r.id));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(format!("{}.json", r.id))).map_err(|e| e.to_string())
}

/// Every kept report; unreadable ones are skipped (and logged).
pub fn load_reports(dir: &std::path::Path) -> Vec<Report> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            match serde_json::from_str(&text) {
                Ok(r) => Some(r),
                Err(err) => {
                    tracing::warn!(file = %e.path().display(), error = %err, "bad job report");
                    None
                }
            }
        })
        .collect()
}

pub fn delete_report(dir: &std::path::Path, id: u64) {
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
}
#[cfg(test)]
mod tests {
    use super::robocopy::{FileClass, Summary};
    use super::*;

    fn p(s: &str) -> FsPath {
        let Ok(Loc::Fs(f)) = Loc::parse(s) else {
            panic!("{s}")
        };
        f
    }

    fn item(s: &str, is_dir: bool, size: u64) -> Item {
        Item {
            path: p(s),
            is_dir,
            size,
        }
    }

    fn req(op: Op, items: Vec<Item>, dest: &str) -> Request {
        Request {
            op,
            items,
            dest: p(dest),
            overwrite: Overwrite::Never,
        }
    }

    #[test]
    fn plan_table() {
        // Small files: native.
        let r = req(
            Op::Copy,
            vec![
                item(r"C:\a\x.txt", false, 10),
                item(r"C:\a\y.txt", false, 10),
            ],
            r"D:\b",
        );
        let Ok(pl) = plan(&r, NATIVE_MAX_BYTES, &[]) else {
            panic!()
        };
        assert_eq!(pl.backend, Backend::Native);
        assert_eq!(pl.pairs[1].dst, r"D:\b\y.txt");
        assert_eq!(pl.title, "2 files → b");

        // A folder or a big file: robocopy, folders first, files grouped by folder.
        let r = req(
            Op::Copy,
            vec![
                item(r"\\srv01\p\HIT\big.bin", false, NATIVE_MAX_BYTES),
                item(r"\\srv01\p\HIT\sub", true, 0),
                item(r"\\srv01\p\HIT\a.txt", false, 1),
            ],
            r"D:\local",
        );
        let Ok(pl) = plan(&r, NATIVE_MAX_BYTES, &[]) else {
            panic!()
        };
        assert_eq!(pl.backend, Backend::Robocopy);
        assert_eq!(pl.steps.len(), 2);
        assert_eq!(
            (pl.steps[0].src.as_str(), pl.steps[0].dst.as_str()),
            (r"\\srv01\p\HIT\sub", r"D:\local\sub")
        );
        assert_eq!(pl.steps[1].files, ["big.bin", "a.txt"]);
        assert_eq!(pl.steps[1].dst, r"D:\local");
        assert_eq!(pl.title, "1 folder, 2 files → local");

        // A move on one volume: rename. Across volumes: robocopy / native.
        let r = req(Op::Move, vec![item(r"C:\a\big", true, 0)], r"C:\b");
        assert_eq!(
            plan(&r, NATIVE_MAX_BYTES, &[]).map(|p| p.backend),
            Ok(Backend::Rename)
        );
        let r = req(Op::Move, vec![item(r"C:\a\big", true, 0)], r"D:\b");
        assert_eq!(
            plan(&r, NATIVE_MAX_BYTES, &[]).map(|p| p.backend),
            Ok(Backend::Robocopy)
        );
        let r = req(
            Op::Move,
            vec![item(r"\\srv01\p\a.txt", false, 3)],
            r"\\SRV01\P\b",
        );
        assert_eq!(
            plan(&r, NATIVE_MAX_BYTES, &[]).map(|p| p.backend),
            Ok(Backend::Rename)
        );

        // Refusals.
        let r = req(Op::Copy, vec![item(r"C:\a", true, 0)], r"C:\a\b");
        assert!(plan(&r, NATIVE_MAX_BYTES, &[]).is_err_and(|e| e.contains("into itself")));
        let r = req(Op::Move, vec![item(r"C:\a\x.txt", false, 1)], r"C:\A");
        assert!(
            plan(&r, NATIVE_MAX_BYTES, &[]).is_err_and(|e| e.contains("already in this folder"))
        );
        assert!(plan(&req(Op::Copy, vec![], r"C:\a"), NATIVE_MAX_BYTES, &[]).is_err());
        let r = req(
            Op::Copy,
            vec![item(r"C:\a\x.txt", false, 1), item(r"C:\a\d", true, 0)],
            r"C:\a",
        );
        assert!(plan(&r, NATIVE_MAX_BYTES, &[]).is_err_and(|e| e.contains("separately")));
    }

    #[test]
    fn free_names_like_yazi() {
        let taken: std::collections::HashSet<String> =
            ["report.pdf", "report_1.pdf", "v1.2", ".env"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(free_name("report.pdf", false, &taken), "report_2.pdf");
        assert_eq!(free_name("notes.txt", false, &taken), "notes_1.txt");
        assert_eq!(
            free_name("v1.2", true, &taken),
            "v1.2_1",
            "a folder keeps its dots"
        );
        assert_eq!(free_name(".env", false, &taken), ".env_1");
        assert_eq!(free_name("Makefile", false, &taken), "Makefile_1");
    }

    /// A copy pasted where its original is: a free name beside it, native for files
    /// (any size), robocopy into the new name for folders.
    #[test]
    fn copy_in_place_gets_a_free_name() {
        let taken = [
            "A.pdf".to_string(),
            "a_1.pdf".into(),
            "big.bin".into(),
            "HIT".into(),
        ];
        let r = req(
            Op::Copy,
            vec![
                item(r"C:\w\a.pdf", false, 1),
                item(r"C:\w\big.bin", false, NATIVE_MAX_BYTES * 4),
                item(r"D:\elsewhere\c.txt", false, 1),
            ],
            r"C:\W",
        );
        let pl = plan(&r, NATIVE_MAX_BYTES, &taken).expect("plan");
        assert_eq!(pl.backend, Backend::Native);
        let dsts: Vec<&str> = pl.pairs.iter().map(|p| p.dst.as_str()).collect();
        assert_eq!(dsts, [r"C:\W\a_2.pdf", r"C:\W\big_1.bin", r"C:\W\c.txt"]);

        let r = req(Op::Copy, vec![item(r"C:\w\HIT", true, 0)], r"C:\w");
        let pl = plan(&r, NATIVE_MAX_BYTES, &taken).expect("plan");
        assert_eq!(pl.backend, Backend::Robocopy);
        assert_eq!(pl.steps[0].dst, r"C:\w\HIT_1");
        assert_eq!(pl.title, "HIT → HIT_1");

        // Two copies of the same name in one paste don't collide either.
        let r = req(
            Op::Copy,
            vec![item(r"C:\w\x.txt", false, 1), item(r"C:\w\x.txt", false, 1)],
            r"C:\w",
        );
        let pl = plan(&r, NATIVE_MAX_BYTES, &["x.txt".to_string()]).expect("plan");
        let dsts: Vec<&str> = pl.pairs.iter().map(|p| p.dst.as_str()).collect();
        assert_eq!(dsts, [r"C:\w\x_1.txt", r"C:\w\x_2.txt"]);
    }

    #[test]
    fn retry_only_failed() {
        let steps = vec![
            Step {
                src: r"\\srv01\p\HIT".into(),
                dst: r"D:\HIT".into(),
                files: vec![],
            },
            Step {
                src: r"C:\a".into(),
                dst: r"D:\b".into(),
                files: vec!["x.txt".into()],
            },
        ];
        let failed = vec![
            r"\\srv01\p\HIT\hitmac\pipe.pmlmac".to_string(),
            r"\\srv01\p\HIT\hitmac\other.pmlmac".to_string(),
            r"\\srv01\p\HIT\top.txt".to_string(),
            r"C:\a\x.txt".to_string(),
            r"E:\unrelated.txt".to_string(),
        ];
        let r = retry_steps(&steps, &failed);
        assert_eq!(r.len(), 3);
        assert_eq!(
            (r[0].src.as_str(), r[0].dst.as_str()),
            (r"\\srv01\p\HIT\hitmac", r"D:\HIT\hitmac")
        );
        assert_eq!(r[0].files, ["pipe.pmlmac", "other.pmlmac"]);
        assert_eq!(
            (r[1].src.as_str(), r[1].files.as_slice()),
            (r"\\srv01\p\HIT", &["top.txt".to_string()][..])
        );
        assert_eq!((r[2].dst.as_str(), r[2].files.len()), (r"D:\b", 1));
    }

    fn file(path: &str, size: u64) -> Event {
        Event::File {
            class: FileClass::New,
            size,
            path: path.into(),
        }
    }

    #[test]
    fn phases_from_scan_to_finishing_to_done() {
        let pl = Plan {
            backend: Backend::Robocopy,
            steps: vec![],
            pairs: vec![],
            title: "HIT → local".into(),
        };
        let mut j = Job::new(
            1,
            &pl,
            Op::Copy,
            r"D:\local".into(),
            Overwrite::Never,
            "x.log".into(),
        );
        let t0 = j.started;
        j.refresh(t0);
        assert_eq!(j.phase, Phase::Scanning);
        j.apply(
            JobEvent::Scanned {
                files: 2,
                bytes: 300,
            },
            t0,
        );
        assert_eq!(j.phase, Phase::Copying);
        j.apply(
            JobEvent::Output(vec![file(r"S:\a", 100), Event::Progress(100.0)]),
            t0,
        );
        j.apply(
            JobEvent::Output(vec![file(r"S:\b", 200), Event::Progress(50.0)]),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(j.tracker.bytes_now(), 200);
        assert!((j.fraction() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(j.speed(t0 + Duration::from_secs(1)), Some(100.0));
        // The ETA uses the per-second smoothed speed: 200 B in the first second.
        assert_eq!(j.history.iter().copied().collect::<Vec<_>>(), [200.0]);
        assert_eq!(
            j.eta(t0 + Duration::from_secs(1)),
            Some(Duration::from_millis(500))
        );
        let t1 = t0 + Duration::from_secs(2);
        j.apply(JobEvent::Output(vec![Event::Progress(100.0)]), t1);
        assert_eq!(
            j.phase,
            Phase::Finishing,
            "all bytes written, still running"
        );
        assert_eq!(j.phase_since, t1);
        let t2 = t1 + Duration::from_secs(47);
        j.apply(JobEvent::StepDone(1), t2);
        j.apply(JobEvent::Finished { cancelled: false }, t2);
        assert_eq!(j.phase, Phase::Done);
        let r = j.report(t2);
        assert_eq!((r.copied, r.bytes, r.failed.len()), (2, 300, 0));
        assert_eq!(r.result_line(), "DONE robocopy exit 1  =  1 (files copied)");
        assert_eq!(clock(j.elapsed(t2)), "0:49");
        // The stepper's times, the chart's samples and ACTIVITY, kept in the report.
        assert_eq!(
            r.stages,
            [
                (Phase::Scanning, 0),
                (Phase::Copying, 2),
                (Phase::Finishing, 47)
            ]
        );
        assert_eq!(r.history.first(), Some(&200));
        let kinds: Vec<ActKind> = r.activity.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            [ActKind::Scan, ActKind::Copy, ActKind::Copy, ActKind::Done]
        );
        assert_eq!(r.activity[0].text, "2 files to copy, 300 B");
        assert_eq!(r.activity[1].text, "a (100 B)");
        assert!(
            r.activity[3].text.starts_with("2 files copied"),
            "{:?}",
            r.activity[3]
        );
        assert_eq!(files_word(1), "1 file");
    }

    #[test]
    fn stage_times_merge_retrying_and_stop_at_the_end() {
        let t = Instant::now();
        let s = |n: u64| t + Duration::from_secs(n);
        let log = [
            (Phase::Scanning, s(0)),
            (Phase::Copying, s(1)),
            (Phase::Retrying, s(5)),
            (Phase::Copying, s(8)),
            (Phase::Finishing, s(20)),
            (Phase::Done, s(22)),
        ];
        assert_eq!(
            stage_times(&log, s(22)),
            [
                (Phase::Scanning, Duration::from_secs(1)),
                (Phase::Copying, Duration::from_secs(19)),
                (Phase::Finishing, Duration::from_secs(2)),
            ]
        );
    }

    #[test]
    fn retrying_failed_and_cancelled() {
        let pl = Plan {
            backend: Backend::Robocopy,
            steps: vec![],
            pairs: vec![],
            title: "t".into(),
        };
        let now = Instant::now();
        let mut j = Job::new(2, &pl, Op::Copy, "D:".into(), Overwrite::Never, "l".into());
        j.apply(JobEvent::Scanned { files: 1, bytes: 6 }, now);
        j.apply(
            JobEvent::Output(vec![
                file(r"S:\a.txt", 6),
                Event::Error(robocopy::RcError {
                    code: 32,
                    action: "Copying File".into(),
                    path: Some(r"S:\a.txt".into()),
                    message: "used by another process".into(),
                }),
            ]),
            now,
        );
        assert_eq!(j.phase, Phase::Retrying);
        j.apply(JobEvent::StepDone(8), now);
        j.apply(JobEvent::Finished { cancelled: false }, now);
        assert_eq!(j.phase, Phase::Failed);
        let r = j.report(now);
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].code, 32);
        assert!(r.result_line().starts_with("FAILED robocopy exit 8"));

        let mut j = Job::new(3, &pl, Op::Copy, "D:".into(), Overwrite::Never, "l".into());
        j.apply(JobEvent::Finished { cancelled: true }, now);
        assert_eq!(j.phase, Phase::Cancelled);
        // A summary alone (nothing to copy) is done, not finishing.
        let mut j = Job::new(4, &pl, Op::Copy, "D:".into(), Overwrite::Never, "l".into());
        j.apply(JobEvent::Scanned { files: 0, bytes: 0 }, now);
        j.apply(
            JobEvent::Output(vec![Event::Summary(Summary::default())]),
            now,
        );
        assert_eq!(j.phase, Phase::Copying);
    }

    #[test]
    fn report_round_trips() {
        let pl = Plan {
            backend: Backend::Native,
            steps: vec![],
            pairs: vec![],
            title: "t".into(),
        };
        let now = Instant::now();
        let mut j = Job::new(5, &pl, Op::Move, "D:".into(), Overwrite::Always, "l".into());
        j.apply(JobEvent::Finished { cancelled: false }, now);
        let r = j.report(now);
        let text = serde_json::to_string(&r).unwrap_or_default();
        let back: Option<Report> = serde_json::from_str(&text).ok();
        assert_eq!(back, Some(r));
    }

    #[test]
    fn formatting() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(4_080_218_931), "3.80 GB");
        assert_eq!(clock(Duration::from_secs(158)), "2:38");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn files_and_paths() {
        assert_eq!(slug("HIT → local"), "HIT-local");
        assert_eq!(slug("3 files → hitmac"), "3-files-hitmac");
        assert_eq!(parent_of(r"C:\a\b.txt"), Some(r"C:\a"));
        assert_eq!(parent_of(r"C:\b.txt"), Some(r"C:\"));
        assert_eq!(parent_of(r"\\srv01\p\b.txt"), Some(r"\\srv01\p"));
        let steps = flat_steps(
            &[
                r"C:\a\x.txt".into(),
                r"C:\a\y.txt".into(),
                r"D:\z.txt".into(),
            ],
            r"E:\out",
        );
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[0].src.as_str(), steps[0].files.len()), (r"C:\a", 2));
        assert_eq!(
            (steps[1].src.as_str(), steps[1].dst.as_str()),
            (r"D:\", r"E:\out")
        );

        let dir = std::env::temp_dir().join(format!("sentier-reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pl = Plan {
            backend: Backend::Native,
            steps: vec![],
            pairs: vec![],
            title: "t".into(),
        };
        let now = Instant::now();
        let mut j = Job::new(42, &pl, Op::Copy, "D:".into(), Overwrite::Never, "l".into());
        j.apply(JobEvent::Finished { cancelled: false }, now);
        let r = j.report(now);
        assert_eq!(save_report(&dir, &r), Ok(()));
        std::fs::write(dir.join("junk.json"), "{").ok();
        assert_eq!(load_reports(&dir), vec![r]);
        delete_report(&dir, 42);
        assert!(load_reports(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
