//! Runs a job's plan (blocking: the worker puts it on the blocking pool) and reports
//! through `send`. robocopy runs as a hidden child process; its stdout is read on its
//! own thread, decoded from the OEM code page, parsed, and sent in batches about every
//! 100 ms. The native and rename backends make the same events, so the app tracks every
//! job the same way.

use super::robocopy::{
    self, Event, FileClass, Options, Overwrite, Parser, RcError, Row, Summary, Tracker,
};
use super::{Backend, JobEvent, Pair, Plan};
use crate::fs::worker::CancelToken;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BATCH_EVERY: Duration = Duration::from_millis(100);
/// Native copies retry a locked file this many times, a second apart.
const NATIVE_RETRIES: u32 = 2;

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
pub fn run(spec: &Spec, cancel: &CancelToken, send: &dyn Fn(JobEvent)) {
    log_line(&spec.log, &format!("sentier job: {}", spec.plan.title));
    let cancelled = match spec.plan.backend {
        Backend::Robocopy => run_robocopy(spec, cancel, send),
        Backend::Native => run_pairs(spec, cancel, send, false),
        Backend::Rename => run_pairs(spec, cancel, send, true),
    };
    log_line(&spec.log, if cancelled { "cancelled" } else { "finished" });
    send(JobEvent::Finished { cancelled });
}

fn log_line(log: &str, line: &str) {
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

/// The `/L` pass for the totals, then each step. Returns whether it was cancelled.
fn run_robocopy(spec: &Spec, cancel: &CancelToken, send: &dyn Fn(JobEvent)) -> bool {
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
        let (t, _) = match dry_run(step, &opts(true), cancel) {
            Ok(r) => r,
            Err(e) => {
                send(JobEvent::Error(e));
                return false;
            }
        };
        if cancel.is_cancelled() {
            return true;
        }
        if let Some(s) = t.summary {
            files += s.files.copied;
            bytes += s.bytes.copied;
        }
    }
    send(JobEvent::Scanned { files, bytes });
    for step in &spec.plan.steps {
        let args = robocopy::args(step, &opts(false));
        log_line(&spec.log, &format!("robocopy {}", quote_args(&args)));
        let r = robocopy_once(&args, cancel, &mut |evs| send(JobEvent::Output(evs)));
        match r {
            Ok(code) => send(JobEvent::StepDone(code)),
            Err(e) => {
                send(JobEvent::Error(e));
                return false;
            }
        }
        if cancel.is_cancelled() {
            return true;
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
    let code = robocopy_once(&robocopy::args(step, &opts), cancel, &mut |evs| {
        for e in evs {
            t.apply(e);
        }
    })?;
    t.finish();
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
/// code (-1 when killed).
fn robocopy_once(
    args: &[String],
    cancel: &CancelToken,
    on_events: &mut dyn FnMut(Vec<Event>),
) -> Result<i32, String> {
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
        if cancel.is_cancelled() && !killed {
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
        -1
    } else {
        status.code().unwrap_or(-1)
    })
}

/// The native (copy) and rename backends: one pair at a time. Returns whether it was
/// cancelled.
fn run_pairs(spec: &Spec, cancel: &CancelToken, send: &dyn Fn(JobEvent), rename: bool) -> bool {
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
    let mut cancelled = false;
    for pair in pairs {
        if cancel.is_cancelled() {
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
        let mut attempt = 0;
        loop {
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
                    break;
                }
                Err(e) => {
                    let code = e.raw_os_error().unwrap_or(0) as u32;
                    log_line(&spec.log, &format!("ERROR {code} {}: {e}", pair.src));
                    send(JobEvent::Output(vec![Event::Error(RcError {
                        code,
                        action: if rename { "Moving" } else { "Copying File" }.into(),
                        path: Some(pair.src.clone()),
                        message: os_message(&e),
                    })]));
                    attempt += 1;
                    // Only a locked file (32, 33) is worth trying again.
                    if attempt > NATIVE_RETRIES || !matches!(code, 32 | 33) || cancel.is_cancelled()
                    {
                        files.failed += 1;
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
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
    cancelled
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
    use crate::jobs::robocopy::Step;
    use crate::jobs::{Job, Op, Phase};
    use std::sync::Mutex;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sentier-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Runs `plan` to the end and returns the job it built.
    fn drive(plan: Plan, moving: bool, overwrite: Overwrite, log: &str) -> Job {
        let spec = Spec {
            plan: plan.clone(),
            moving,
            overwrite,
            threads: 0,
            log: log.to_string(),
            mirror: false,
            exclude_dirs: vec![],
            exclude_files: vec![],
            totals: None,
        };
        let events = Mutex::new(Vec::new());
        run(&spec, &CancelToken::default(), &|e| {
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

    #[test]
    fn robocopy_copies_a_tree_and_skips_existing() {
        let dir = temp("rc");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub")).ok();
        std::fs::write(src.join("a.txt"), "hello").ok();
        std::fs::write(src.join("sub").join("é.txt"), "é").ok();
        let dst = dir.join("dst").join("src");
        let plan = Plan {
            backend: Backend::Robocopy,
            steps: vec![Step {
                src: src.display().to_string(),
                dst: dst.display().to_string(),
                files: vec![],
            }],
            pairs: vec![],
            title: "src → dst".into(),
        };
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
