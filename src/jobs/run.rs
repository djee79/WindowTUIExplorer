//! Runs a job's plan (blocking: the worker puts it on the blocking pool) and reports
//! through `send`. robocopy runs as a hidden child process; its stdout is read on its
//! own thread, decoded from the OEM code page, parsed, and sent in batches about every
//! 100 ms. The native and rename backends make the same events, so the app tracks every
//! job the same way.
//!
//! **sentier owns retries** (docs/JOBS_PANEL_SPEC.md): robocopy runs with `/R:0`, so a
//! locked file (error 32 / 33) fails at once and everything else is copied first. The
//! locked files are then tried again, one by one, every [`RETRY_EVERY`], up to
//! [`RETRY_TRIES`] times. While it waits the panel shows why, and the user can skip the
//! file (`s`), try it now (`R`), pause or cancel.
//!
//! **Pause** (`p`) stops robocopy (its file handles and the server's locks go with it);
//! resuming runs the same step again, which skips what's already copied and restarts a
//! half-copied file where it stopped (`/Z`). The native backend pauses between files.

use super::robocopy::{
    self, Event, FileClass, Options, Overwrite, Parser, RcError, Row, Step, Summary, Tracker,
};
use super::{Backend, Control, JobEvent, Pair, Plan};
use crate::fs::worker::CancelToken;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BATCH_EVERY: Duration = Duration::from_millis(100);
/// How often, and how many times, sentier tries a locked file again.
pub const RETRY_TRIES: u32 = 5;
pub const RETRY_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(150)
} else {
    Duration::from_secs(10)
};
/// How often the runner looks at the panel's flags while waiting.
const POLL: Duration = Duration::from_millis(50);

pub struct Spec {
    pub plan: Plan,
    pub moving: bool,
    pub overwrite: Overwrite,
    /// `/MT:n`, 0 = off.
    pub threads: u32,
    pub log: String,
    /// Sync presets: `/MIR` instead of `/E`.
    pub mirror: bool,
    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    /// (files, bytes) already known from a preview: the `/L` pass is skipped.
    pub totals: Option<(u64, u64)>,
}

/// Runs everything, then sends `Finished`.
pub fn run(spec: &Spec, ctl: &Control, send: &dyn Fn(JobEvent)) {
    log_line(&spec.log, &format!("sentier job: {}", spec.plan.title));
    let cancelled = match spec.plan.backend {
        Backend::Robocopy => run_robocopy(spec, ctl, send),
        Backend::Native => run_pairs(spec, ctl, send, false),
        Backend::Rename => run_pairs(spec, ctl, send, true),
    };
    log_line(&spec.log, if cancelled { "cancelled" } else { "finished" });
    send(JobEvent::Finished { cancelled });
}

pub(super) fn log_line(log: &str, line: &str) {
    if let Some(dir) = std::path::Path::new(log).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "{now}  {line}");
    }
}

/// While the job is paused: waits, telling the panel. False when it got cancelled.
fn hold(ctl: &Control, send: &dyn Fn(JobEvent), log: &str) -> bool {
    if ctl.is_cancelled() {
        return false;
    }
    if !ctl.paused() {
        return true;
    }
    send(JobEvent::Paused(true));
    log_line(log, "paused");
    while ctl.paused() {
        if ctl.is_cancelled() {
            return false;
        }
        std::thread::sleep(POLL);
    }
    send(JobEvent::Paused(false));
    log_line(log, "resumed");
    !ctl.is_cancelled()
}

/// A file that was locked, and how to try it again.
struct Locked {
    path: String,
    code: u32,
    message: String,
    again: Again,
}

enum Again {
    /// robocopy on just that file (its folder, the file name as the filter).
    Robocopy(Step),
    Native(Pair),
}

fn is_locked(code: u32) -> bool {
    matches!(code, 32 | 33)
}

/// The one-file step for `path`, a file under `step`'s source.
fn single_file_step(step: &Step, path: &str) -> Option<Step> {
    let base = step.src.trim_end_matches('\\');
    let head = path.get(..base.len())?;
    if !head.eq_ignore_ascii_case(base) {
        return None;
    }
    let rel = path[base.len()..].trim_start_matches('\\');
    let (dir, name) = rel.rsplit_once('\\').unwrap_or(("", rel));
    let join = |a: &str| {
        let a = a.trim_end_matches('\\');
        if dir.is_empty() {
            a.to_string()
        } else {
            format!("{a}\\{dir}")
        }
    };
    (!name.is_empty()).then(|| Step {
        src: join(&step.src),
        dst: join(&step.dst),
        files: vec![name.to_string()],
    })
}

/// The `/L` pass for the totals, then each step, then the locked files. Returns whether
/// it was cancelled.
pub(super) fn run_robocopy(spec: &Spec, ctl: &Control, send: &dyn Fn(JobEvent)) -> bool {
    let opts = |list_only: bool| Options {
        moving: spec.moving,
        overwrite: spec.overwrite,
        mirror: spec.mirror,
        list_only,
        threads: if list_only { 0 } else { spec.threads },
        log: (!list_only).then(|| spec.log.clone()),
        exclude_dirs: spec.exclude_dirs.clone(),
        exclude_files: spec.exclude_files.clone(),
    };
    // A preview already counted it: no second `/L` pass.
    let (mut files, mut bytes) = spec.totals.unwrap_or((0, 0));
    let to_scan = if spec.totals.is_some() {
        &[][..]
    } else {
        &spec.plan.steps[..]
    };
    for step in to_scan {
        let (t, _) = match dry_run(step, &opts(true), &ctl.cancel) {
            Ok(r) => r,
            Err(e) => {
                send(JobEvent::Error(e));
                return false;
            }
        };
        if ctl.is_cancelled() {
            return true;
        }
        if let Some(s) = t.summary {
            files += s.files.copied;
            bytes += s.bytes.copied;
        }
    }
    send(JobEvent::Scanned { files, bytes });
    let mut locked: Vec<Locked> = Vec::new();
    for step in &spec.plan.steps {
        // Paused mid-step: robocopy is stopped, and the step runs again on resume.
        loop {
            if !hold(ctl, send, &spec.log) {
                return true;
            }
            let args = robocopy::args(step, &opts(false));
            log_line(&spec.log, &format!("robocopy {}", quote_args(&args)));
            let mut seen: Vec<RcError> = Vec::new();
            let r = robocopy_once(&args, &|| ctl.is_cancelled() || ctl.paused(), &mut |evs| {
                for e in &evs {
                    if let Event::Error(x) = e
                        && is_locked(x.code)
                        && x.path.is_some()
                    {
                        seen.push(x.clone());
                    }
                }
                send(JobEvent::Output(evs));
            });
            match r {
                Err(e) => {
                    send(JobEvent::Error(e));
                    return false;
                }
                Ok((_, true)) if ctl.is_cancelled() => return true,
                Ok((_, true)) => {
                    log_line(&spec.log, "robocopy stopped (paused); it resumes from here");
                }
                Ok((code, false)) => {
                    send(JobEvent::StepDone(code));
                    for x in seen {
                        let Some(path) = x.path.clone() else { continue };
                        if locked.iter().any(|l| l.path.eq_ignore_ascii_case(&path)) {
                            continue;
                        }
                        if let Some(one) = single_file_step(step, &path) {
                            locked.push(Locked {
                                path,
                                code: x.code,
                                message: x.message,
                                again: Again::Robocopy(one),
                            });
                        }
                    }
                    break;
                }
            }
        }
    }
    retry_locked(spec, ctl, send, locked, &opts(false))
}

/// How a wait for the next try ended.
enum Wait {
    Due,
    Now,
    Skip,
    /// Paused and resumed: start the countdown again.
    Again,
    Cancelled,
}

fn countdown(ctl: &Control, send: &dyn Fn(JobEvent), log: &str, until: Instant) -> Wait {
    loop {
        if ctl.is_cancelled() {
            return Wait::Cancelled;
        }
        if ctl.take_skip() {
            return Wait::Skip;
        }
        if ctl.take_retry_now() {
            return Wait::Now;
        }
        if ctl.paused() {
            return if hold(ctl, send, log) {
                Wait::Again
            } else {
                Wait::Cancelled
            };
        }
        if Instant::now() >= until {
            return Wait::Due;
        }
        std::thread::sleep(POLL);
    }
}

/// How one more try went.
enum Try {
    Ok,
    Locked(u32, String),
    Failed,
    Paused,
    Cancelled,
}

fn try_again(
    item: &Locked,
    spec: &Spec,
    ctl: &Control,
    send: &dyn Fn(JobEvent),
    opts: &Options,
) -> Try {
    match &item.again {
        Again::Robocopy(step) => {
            // Never /MIR with a file filter: only this one file.
            let o = Options {
                mirror: false,
                ..opts.clone()
            };
            let args = robocopy::args(step, &o);
            log_line(&spec.log, &format!("retry: robocopy {}", quote_args(&args)));
            let mut err: Option<RcError> = None;
            let r = robocopy_once(&args, &|| ctl.is_cancelled() || ctl.paused(), &mut |evs| {
                for e in &evs {
                    if let Event::Error(x) = e {
                        err = Some(x.clone());
                    }
                }
                send(JobEvent::Output(evs));
            });
            match r {
                Err(e) => {
                    log_line(&spec.log, &format!("retry: {e}"));
                    Try::Failed
                }
                Ok((_, true)) if ctl.is_cancelled() => Try::Cancelled,
                Ok((_, true)) => Try::Paused,
                Ok((code, false)) => match err {
                    Some(x) if is_locked(x.code) => Try::Locked(x.code, x.message),
                    Some(_) => Try::Failed,
                    None if code >= 8 => Try::Failed,
                    None => Try::Ok,
                },
            }
        }
        Again::Native(pair) => {
            send(JobEvent::Output(vec![Event::File {
                class: FileClass::New,
                size: pair.size,
                path: pair.src.clone(),
            }]));
            match copy_pair(pair, spec.moving) {
                Ok(()) => {
                    send(JobEvent::Output(vec![Event::Progress(100.0)]));
                    Try::Ok
                }
                Err(e) => {
                    let code = e.raw_os_error().unwrap_or(0) as u32;
                    let message = os_message(&e);
                    send(JobEvent::Output(vec![Event::Error(RcError {
                        code,
                        action: "Copying File".into(),
                        path: Some(pair.src.clone()),
                        message: message.clone(),
                    })]));
                    if is_locked(code) {
                        Try::Locked(code, message)
                    } else {
                        Try::Failed
                    }
                }
            }
        }
    }
}

/// The locked files, one by one: wait, try again, until it works, the user skips it, or
/// the tries run out. Returns whether the job was cancelled.
fn retry_locked(
    spec: &Spec,
    ctl: &Control,
    send: &dyn Fn(JobEvent),
    locked: Vec<Locked>,
    opts: &Options,
) -> bool {
    if locked.is_empty() {
        return false;
    }
    // A key pressed before anything was waiting doesn't count.
    ctl.take_skip();
    ctl.take_retry_now();
    log_line(
        &spec.log,
        &format!(
            "{} locked file(s): trying again every {} s, up to {} times",
            locked.len(),
            RETRY_EVERY.as_secs(),
            RETRY_TRIES
        ),
    );
    'item: for item in locked {
        let (mut code, mut message) = (item.code, item.message.clone());
        let mut attempt = 0;
        loop {
            if attempt >= RETRY_TRIES {
                log_line(&spec.log, &format!("gave up (still locked): {}", item.path));
                send(JobEvent::GaveUp {
                    path: item.path.clone(),
                    attempts: attempt,
                });
                continue 'item;
            }
            attempt += 1;
            loop {
                let next = Instant::now() + RETRY_EVERY;
                send(JobEvent::Waiting {
                    path: item.path.clone(),
                    code,
                    message: message.clone(),
                    attempt,
                    max: RETRY_TRIES,
                    next,
                });
                match countdown(ctl, send, &spec.log, next) {
                    Wait::Due | Wait::Now => break,
                    Wait::Again => continue,
                    Wait::Cancelled => return true,
                    Wait::Skip => {
                        log_line(&spec.log, &format!("skipped (s): {}", item.path));
                        send(JobEvent::Skipped(item.path.clone()));
                        continue 'item;
                    }
                }
            }
            match try_again(&item, spec, ctl, send, opts) {
                Try::Ok => {
                    log_line(
                        &spec.log,
                        &format!("copied on retry {attempt}: {}", item.path),
                    );
                    send(JobEvent::Recovered {
                        path: item.path.clone(),
                        attempts: attempt,
                    });
                    continue 'item;
                }
                Try::Locked(c, m) => (code, message) = (c, m),
                Try::Failed => {
                    send(JobEvent::GaveUp {
                        path: item.path.clone(),
                        attempts: attempt,
                    });
                    continue 'item;
                }
                // A pause during the try doesn't use up a try.
                Try::Paused => {
                    attempt -= 1;
                    if !hold(ctl, send, &spec.log) {
                        return true;
                    }
                }
                Try::Cancelled => return true,
            }
        }
    }
    false
}

/// One `/L` pass (`opts.list_only` is forced on): what robocopy would do, and its exit
/// code. Used for a paste's totals and a sync preset's preview.
pub fn dry_run(
    step: &robocopy::Step,
    opts: &Options,
    cancel: &CancelToken,
) -> Result<(Tracker, i32), String> {
    let opts = Options {
        list_only: true,
        threads: 0,
        log: None,
        ..opts.clone()
    };
    let mut t = Tracker::new(true);
    let (code, _) = robocopy_once(
        &robocopy::args(step, &opts),
        &|| cancel.is_cancelled(),
        &mut |evs| {
            for e in evs {
                t.apply(e);
            }
        },
    )?;
    t.finish();
    Ok((t, code))
}

/// [`dry_run`] over several steps (a preset with `include`) into one tracker: what each
/// would do, the summaries added up, the exit codes OR'd.
pub fn dry_run_steps(
    steps: &[robocopy::Step],
    opts: &Options,
    cancel: &CancelToken,
) -> Result<(Tracker, i32), String> {
    let opts = Options {
        list_only: true,
        threads: 0,
        log: None,
        ..opts.clone()
    };
    let mut t = Tracker::new(true);
    let mut code = 0;
    let mut sum = Summary::default();
    let add = |a: &mut Row, b: &Row| {
        a.total += b.total;
        a.copied += b.copied;
        a.skipped += b.skipped;
        a.mismatch += b.mismatch;
        a.failed += b.failed;
        a.extras += b.extras;
    };
    for step in steps {
        let (c, _) = robocopy_once(
            &robocopy::args(step, &opts),
            &|| cancel.is_cancelled(),
            &mut |evs| {
                for e in evs {
                    t.apply(e);
                }
            },
        )?;
        code |= c.max(0);
        if let Some(s) = t.summary.take() {
            add(&mut sum.dirs, &s.dirs);
            add(&mut sum.files, &s.files);
            add(&mut sum.bytes, &s.bytes);
        }
        if cancel.is_cancelled() {
            break;
        }
    }
    t.finish();
    t.summary = Some(sum);
    Ok((t, code))
}

fn quote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One robocopy run: its parsed output goes to `on_events` in batches. Returns the exit
/// code (-1 when killed) and whether it was killed (`stop` said so: cancel or pause).
fn robocopy_once(
    args: &[String],
    stop: &dyn Fn() -> bool,
    on_events: &mut dyn FnMut(Vec<Event>),
) -> Result<(i32, bool), String> {
    let mut child = Command::new("robocopy")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("robocopy didn't start: {e}"))?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Some(out) = stdout.as_mut() {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut parser = Parser::new();
    let mut batch: Vec<Event> = Vec::new();
    let mut last_send = Instant::now();
    let mut killed = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(bytes) => batch.extend(parser.feed(&crate::platform::oem_to_string(&bytes))),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if stop() && !killed {
            let _ = child.kill();
            killed = true;
        }
        if !batch.is_empty() && last_send.elapsed() >= BATCH_EVERY {
            on_events(std::mem::take(&mut batch));
            last_send = Instant::now();
        }
    }
    batch.extend(parser.finish());
    if !batch.is_empty() {
        on_events(batch);
    }
    let _ = reader.join();
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok(if killed {
        (-1, true)
    } else {
        (status.code().unwrap_or(-1), false)
    })
}

/// The native (copy) and rename backends: one pair at a time, pausing between files.
/// A locked file is queued and tried again at the end. Returns whether it was cancelled.
fn run_pairs(spec: &Spec, ctl: &Control, send: &dyn Fn(JobEvent), rename: bool) -> bool {
    let pairs = &spec.plan.pairs;
    send(JobEvent::Scanned {
        files: pairs.len() as u64,
        bytes: if rename {
            0
        } else {
            pairs.iter().map(|p| p.size).sum()
        },
    });
    let mut files = Row {
        total: pairs.len() as u64,
        ..Row::default()
    };
    let mut locked: Vec<Locked> = Vec::new();
    let mut cancelled = false;
    for pair in pairs {
        if !hold(ctl, send, &spec.log) {
            cancelled = true;
            break;
        }
        let exists = std::path::Path::new(&pair.dst).exists();
        if exists && spec.overwrite == Overwrite::Never {
            files.skipped += 1;
            log_line(&spec.log, &format!("skipped (already there): {}", pair.dst));
            continue;
        }
        let size = if rename { 0 } else { pair.size };
        send(JobEvent::Output(vec![Event::File {
            class: FileClass::New,
            size,
            path: pair.src.clone(),
        }]));
        let r = if rename {
            rename_pair(pair, exists)
        } else {
            copy_pair(pair, spec.moving)
        };
        match r {
            Ok(()) => {
                files.copied += 1;
                log_line(&spec.log, &format!("ok: {} → {}", pair.src, pair.dst));
                send(JobEvent::Output(vec![Event::Progress(100.0)]));
            }
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0) as u32;
                let message = os_message(&e);
                log_line(&spec.log, &format!("ERROR {code} {}: {e}", pair.src));
                send(JobEvent::Output(vec![Event::Error(RcError {
                    code,
                    action: if rename { "Moving" } else { "Copying File" }.into(),
                    path: Some(pair.src.clone()),
                    message: message.clone(),
                })]));
                files.failed += 1;
                // Only a locked copy is worth trying again, later.
                if is_locked(code) && !rename {
                    locked.push(Locked {
                        path: pair.src.clone(),
                        code,
                        message,
                        again: Again::Native(pair.clone()),
                    });
                }
            }
        }
    }
    send(JobEvent::Output(vec![Event::Summary(Summary {
        files,
        ..Summary::default()
    })]));
    send(JobEvent::StepDone(if files.failed > 0 {
        8
    } else {
        (files.copied > 0) as i32
    }));
    if cancelled {
        return true;
    }
    let opts = Options {
        moving: spec.moving,
        overwrite: spec.overwrite,
        mirror: false,
        list_only: false,
        threads: 0,
        log: None,
        exclude_dirs: Vec::new(),
        exclude_files: Vec::new(),
    };
    retry_locked(spec, ctl, send, locked, &opts)
}

/// The message without std's ` (os error 32)` tail.
fn os_message(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn copy_pair(p: &Pair, moving: bool) -> std::io::Result<()> {
    std::fs::copy(&p.src, &p.dst)?;
    if moving {
        std::fs::remove_file(&p.src)?;
    }
    Ok(())
}

/// `P` over an existing target: the target goes first (rename won't replace a folder).
fn rename_pair(p: &Pair, exists: bool) -> std::io::Result<()> {
    if exists {
        let dst = std::path::Path::new(&p.dst);
        if dst.is_dir() {
            std::fs::remove_dir_all(dst)?;
        } else {
            std::fs::remove_file(dst)?;
        }
    }
    std::fs::rename(&p.src, &p.dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{Job, Op, Phase};
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::Mutex;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sentier-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn spec(plan: &Plan, moving: bool, overwrite: Overwrite, log: &str) -> Spec {
        Spec {
            plan: plan.clone(),
            moving,
            overwrite,
            threads: 0,
            log: log.to_string(),
            mirror: false,
            exclude_dirs: vec![],
            exclude_files: vec![],
            totals: None,
        }
    }

    /// Runs `plan` to the end with `ctl`, calling `on` for every event (to press keys
    /// as the panel would), and returns the job it built.
    fn drive_with(
        plan: Plan,
        moving: bool,
        overwrite: Overwrite,
        log: &str,
        ctl: &Control,
        on: &(dyn Fn(&JobEvent) + Sync),
    ) -> Job {
        let spec = spec(&plan, moving, overwrite, log);
        let events = Mutex::new(Vec::new());
        run(&spec, ctl, &|e| {
            on(&e);
            if let Ok(mut v) = events.lock() {
                v.push(e);
            }
        });
        let op = if moving { Op::Move } else { Op::Copy };
        let mut job = Job::new(1, &plan, op, "dest".into(), overwrite, log.into());
        for e in events.into_inner().unwrap_or_default() {
            job.apply(e, Instant::now());
        }
        job
    }

    fn drive(plan: Plan, moving: bool, overwrite: Overwrite, log: &str) -> Job {
        drive_with(plan, moving, overwrite, log, &Control::default(), &|_| {})
    }

    /// Opens `path` with no sharing, as an open AVEVA database does: robocopy and
    /// std::fs::copy get error 32 until it's dropped.
    fn lock(path: &std::path::Path) -> Option<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
            .ok()
    }

    fn tree_plan(src: &std::path::Path, dst: &std::path::Path) -> Plan {
        Plan {
            backend: Backend::Robocopy,
            steps: vec![Step {
                src: src.display().to_string(),
                dst: dst.display().to_string(),
                files: vec![],
            }],
            pairs: vec![],
            title: "src → dst".into(),
        }
    }

    #[test]
    fn robocopy_copies_a_tree_and_skips_existing() {
        let dir = temp("rc");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub")).ok();
        std::fs::write(src.join("a.txt"), "hello").ok();
        std::fs::write(src.join("sub").join("é.txt"), "é").ok();
        let dst = dir.join("dst").join("src");
        let plan = tree_plan(&src, &dst);
        let log = dir.join("job.log").display().to_string();
        let job = drive(plan.clone(), false, Overwrite::Never, &log);
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        assert_eq!((job.total_files, job.tracker.files_done), (Some(2), 2));
        assert_eq!(
            std::fs::read_to_string(dst.join("sub").join("é.txt"))
                .ok()
                .as_deref(),
            Some("é")
        );
        assert!(
            job.tracker
                .listed
                .iter()
                .any(|(_, _, p)| p.ends_with("é.txt")),
            "OEM output decoded: {:?}",
            job.tracker.listed
        );
        let text = std::fs::read(&log).unwrap_or_default();
        assert!(!text.is_empty(), "robocopy and sentier both log");

        // Again: everything is there, nothing to copy.
        let job = drive(plan, false, Overwrite::Never, &log);
        assert_eq!(job.phase, Phase::Done);
        assert_eq!((job.total_files, job.tracker.files_done), (Some(0), 0));
        assert_eq!(job.exit, Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The headline: a locked file doesn't hold the copy up. Everything else is copied,
    /// sentier waits and says why, and the file goes through once it's released.
    #[test]
    fn robocopy_locked_file_is_retried_by_sentier_until_released() {
        let dir = temp("rc-lock");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("dbs")).ok();
        std::fs::write(src.join("a.txt"), "a").ok();
        std::fs::write(src.join("dbs").join("demo1043.dbx"), "db").ok();
        let dst = dir.join("dst");
        let held = Mutex::new(lock(&src.join("dbs").join("demo1043.dbx")));
        let log = dir.join("job.log").display().to_string();
        let ctl = Control::default();
        let job = drive_with(
            tree_plan(&src, &dst),
            false,
            Overwrite::Never,
            &log,
            &ctl,
            &|e| {
                // The user closes the file after the second wait starts.
                if let JobEvent::Waiting { attempt: 2, .. } = e
                    && let Ok(mut h) = held.lock()
                {
                    h.take();
                }
            },
        );
        assert_eq!(
            job.phase,
            Phase::Done,
            "{:?} {:?}",
            job.error,
            job.tracker.failed
        );
        assert!(dst.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst.join("dbs").join("demo1043.dbx"))
                .ok()
                .as_deref(),
            Some("db"),
            "copied on the retry, into its own folder"
        );
        assert!(job.tracker.failed.is_empty());
        assert_eq!(job.retry_attempts, 2);
        assert_eq!(job.exit.map(|c| c & 8), Some(0), "no stale 'copies failed'");
        let r = job.report(Instant::now());
        assert_eq!(r.retried.len(), 1);
        assert!(
            r.activity
                .iter()
                .any(|a| a.text.contains("demo1043.dbx copied on retry 2")),
            "{:?}",
            r.activity
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `s` on a file that stays locked: left out, the job ends with it listed as skipped.
    #[test]
    fn skip_leaves_a_locked_file_out_and_retry_now_skips_the_wait() {
        let dir = temp("rc-skip");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).ok();
        std::fs::write(src.join("a.txt"), "a").ok();
        std::fs::write(src.join("busy.dbx"), "x").ok();
        let _held = lock(&src.join("busy.dbx"));
        let log = dir.join("job.log").display().to_string();
        let ctl = Control::default();
        let ctl2 = ctl.clone();
        let job = drive_with(
            tree_plan(&src, &dir.join("dst")),
            false,
            Overwrite::Never,
            &log,
            &ctl,
            &move |e| match e {
                JobEvent::Waiting { attempt: 1, .. } => ctl2.retry_now(),
                JobEvent::Waiting { attempt: 2, .. } => ctl2.skip(),
                _ => {}
            },
        );
        assert_eq!(job.phase, Phase::Done, "{:?}", job.tracker.failed);
        assert_eq!(job.skipped_files.len(), 1);
        assert!(job.skipped_files[0].ends_with("busy.dbx"));
        assert!(job.tracker.failed.is_empty());
        assert!(dir.join("dst").join("a.txt").exists());
        let r = job.report(Instant::now());
        assert_eq!(r.skipped_files.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Still locked after every try: the job fails with that file, the rest copied.
    #[test]
    fn a_file_locked_throughout_fails_after_the_tries() {
        let dir = temp("native-lock");
        std::fs::create_dir_all(dir.join("dst")).ok();
        std::fs::write(dir.join("a.txt"), "a").ok();
        std::fs::write(dir.join("b.dbx"), "b").ok();
        let _held = lock(&dir.join("b.dbx"));
        let pair = |n: &str| Pair {
            src: dir.join(n).display().to_string(),
            dst: dir.join("dst").join(n).display().to_string(),
            size: 1,
            is_dir: false,
        };
        let plan = Plan {
            backend: Backend::Native,
            steps: vec![],
            pairs: vec![pair("b.dbx"), pair("a.txt")],
            title: "2 files → dst".into(),
        };
        let log = dir.join("job.log").display().to_string();
        let job = drive(plan, false, Overwrite::Never, &log);
        assert_eq!(job.phase, Phase::Failed);
        assert!(
            dir.join("dst").join("a.txt").exists(),
            "the rest wasn't held up"
        );
        assert_eq!(job.tracker.failed.len(), 1, "listed once");
        assert_eq!(job.tracker.failed[0].code, 32);
        assert_eq!(job.retry_attempts, RETRY_TRIES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Paused before it starts: it waits (and says so) until resumed, then completes.
    #[test]
    fn pause_waits_and_resume_carries_on() {
        let dir = temp("pause");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).ok();
        std::fs::write(src.join("a.txt"), "a").ok();
        let log = dir.join("job.log").display().to_string();
        let ctl = Control::default();
        ctl.set_paused(true);
        let resume = ctl.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            resume.set_paused(false);
        });
        let paused_seen = Mutex::new(Vec::new());
        let job = drive_with(
            tree_plan(&src, &dir.join("dst")),
            false,
            Overwrite::Never,
            &log,
            &ctl,
            &|e| {
                if let JobEvent::Paused(on) = e
                    && let Ok(mut v) = paused_seen.lock()
                {
                    v.push(*on);
                }
            },
        );
        let _ = t.join();
        assert_eq!(paused_seen.into_inner().unwrap_or_default(), [true, false]);
        assert_eq!(job.phase, Phase::Done);
        assert!(!job.paused);
        assert!(dir.join("dst").join("a.txt").exists());

        // Cancelled while paused: it stops, nothing copied.
        let ctl = Control::default();
        ctl.set_paused(true);
        let c = ctl.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c.cancel();
        });
        let job = drive_with(
            tree_plan(&src, &dir.join("dst2")),
            false,
            Overwrite::Never,
            &log,
            &ctl,
            &|_| {},
        );
        let _ = t.join();
        assert_eq!(job.phase, Phase::Cancelled);
        assert!(!dir.join("dst2").join("a.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_file_steps_keep_the_folder() {
        let step = Step {
            src: r"\\srv01\p\HIT".into(),
            dst: r"D:\local\HIT".into(),
            files: vec![],
        };
        assert_eq!(
            single_file_step(&step, r"\\srv01\p\HIT\DEMO\dbs\demo1043.dbx"),
            Some(Step {
                src: r"\\srv01\p\HIT\DEMO\dbs".into(),
                dst: r"D:\local\HIT\DEMO\dbs".into(),
                files: vec!["demo1043.dbx".into()],
            })
        );
        assert_eq!(
            single_file_step(&step, r"\\srv01\p\hit\a.txt").map(|s| s.src),
            Some(r"\\srv01\p\HIT".into()),
            "case-insensitive"
        );
        assert_eq!(single_file_step(&step, r"\\srv01\p\OTHER\a.txt"), None);
    }

    #[test]
    fn native_copy_move_and_rename() {
        let dir = temp("native");
        std::fs::create_dir_all(dir.join("dst")).ok();
        std::fs::write(dir.join("a.txt"), "a").ok();
        std::fs::write(dir.join("b.txt"), "b").ok();
        std::fs::write(dir.join("dst").join("b.txt"), "old").ok();
        let pair = |n: &str| Pair {
            src: dir.join(n).display().to_string(),
            dst: dir.join("dst").join(n).display().to_string(),
            size: 1,
            is_dir: false,
        };
        let plan = Plan {
            backend: Backend::Native,
            steps: vec![],
            pairs: vec![pair("a.txt"), pair("b.txt")],
            title: "2 files → dst".into(),
        };
        let log = dir.join("job.log").display().to_string();
        let job = drive(plan.clone(), false, Overwrite::Never, &log);
        assert_eq!(job.phase, Phase::Done);
        assert_eq!(job.tracker.files_done, 1);
        let r = job.report(Instant::now());
        assert_eq!((r.copied, r.skipped), (1, 1), "b.txt was there already");
        assert_eq!(
            std::fs::read_to_string(dir.join("dst").join("b.txt"))
                .ok()
                .as_deref(),
            Some("old")
        );

        // P with a move: replaces, and the sources are gone.
        let job = drive(plan, true, Overwrite::Always, &log);
        assert_eq!(job.phase, Phase::Done);
        assert_eq!(
            std::fs::read_to_string(dir.join("dst").join("b.txt"))
                .ok()
                .as_deref(),
            Some("b")
        );
        assert!(!dir.join("a.txt").exists() && !dir.join("b.txt").exists());

        // A missing source fails with the OS message.
        let plan = Plan {
            backend: Backend::Rename,
            steps: vec![],
            pairs: vec![pair("gone.txt")],
            title: "t".into(),
        };
        let job = drive(plan, true, Overwrite::Never, &log);
        assert_eq!(job.phase, Phase::Failed);
        assert_eq!(job.tracker.failed.len(), 1);
        assert_eq!(job.tracker.failed[0].code, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
