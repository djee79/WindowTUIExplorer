//! `sentier run "<pin name>"` (CLAUDE.md section 9): runs a sync or backup preset with no
//! TUI, for Windows Task Scheduler.
//!
//! - No preview and no questions. A sync runs with **update** semantics unless
//!   `--allow-mirror` is given, so a scheduled task can't delete local files by surprise.
//! - It writes the same job log and report (`%LOCALAPPDATA%\sentier\jobs\`, so the TUI's
//!   jobs panel lists it) and updates the preset's last run in `presets.json`.
//! - The exit code is robocopy-style: 0–7 ok (1 = something was copied / a zip was
//!   made), 8 a failure or a cancel, 16 nothing could run (unknown preset, bad pin…).
//!
//! The presets are matched by name, exactly (case aside): a scheduled task should never
//! run a different preset because a new one happens to match too.

use super::preset::{LastRun, PresetRef, SyncMode, SyncPreset};
use super::robocopy::Overwrite;
use super::run::Spec;
use super::zip::{BackupPreset, BackupSpec, BackupState, BackupStep};
use super::{Backend, Job, JobEvent, Op, Phase, Plan, Report, clock, human_bytes};
use crate::fs::worker::CancelToken;
use crate::pins::{PinKind, PinStore};
use std::cell::RefCell;
use std::io::Write;
use std::time::{Duration, Instant};

/// Nothing could run: robocopy's "fatal" bit.
pub const EXIT_FATAL: i32 = 16;
/// The run failed or was cancelled.
pub const EXIT_FAILED: i32 = 8;

pub const USAGE: &str = "\
usage: sentier run <preset name> [--allow-mirror] [--threads n]

  Runs a sync or backup preset from pins.toml without the TUI (Task Scheduler).
  The name must match exactly (case aside). With no name, lists the presets.

  --allow-mirror     let a mirror-mode sync delete local files not on the server
                     (without it, a mirror preset runs as update)
  --threads n        robocopy /MT:n

  exit codes: 0-7 ok (1 = copied / zip made), 8 failed or cancelled, 16 couldn't run
";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunArgs {
    pub name: Option<String>,
    pub allow_mirror: bool,
    pub threads: u32,
}

/// Parses what follows `run`. `Ok(None)` means help was asked for.
pub fn parse_args(args: &[String]) -> Result<Option<RunArgs>, String> {
    let mut out = RunArgs::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(None),
            "--allow-mirror" => out.allow_mirror = true,
            "--threads" => {
                out.threads = it
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|n| (1..=128).contains(n))
                    .ok_or("--threads needs a number from 1 to 128")?;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            name => {
                if out.name.replace(name.to_string()).is_some() {
                    return Err("one preset name, please (quote names with spaces)".into());
                }
            }
        }
    }
    Ok(Some(out))
}

/// Lists the presets, for a run without a name or with an unknown one.
fn list(store: &PinStore, out: &mut dyn Write) {
    let presets: Vec<_> = store
        .pins
        .iter()
        .filter(|p| matches!(p.kind, PinKind::Sync | PinKind::Backup))
        .collect();
    if presets.is_empty() {
        let _ = writeln!(out, "no sync or backup presets in pins.toml");
        return;
    }
    let _ = writeln!(out, "presets:");
    for p in presets {
        let _ = writeln!(out, "  {:<7} {}", p.kind.label(), p.name);
    }
}

/// Runs the preset named in `args` from `store` and returns the exit code. Progress and
/// the result go to `out`.
pub fn run(store: &PinStore, args: &RunArgs, out: &mut dyn Write) -> i32 {
    let Some(name) = args.name.as_deref() else {
        list(store, out);
        return EXIT_FATAL;
    };
    let lower = name.trim().to_lowercase();
    let Some(pin) = store.pins.iter().find(|p| {
        matches!(p.kind, PinKind::Sync | PinKind::Backup) && p.name.to_lowercase() == lower
    }) else {
        let _ = writeln!(out, "sentier run: no sync or backup preset named {name:?}");
        list(store, out);
        return EXIT_FATAL;
    };
    let prepared = match pin.kind {
        PinKind::Sync => SyncPreset::from_pin(pin).map(|p| prepare_sync(p, args, out)),
        _ => BackupPreset::from_pin(pin).and_then(|p| prepare_backup(p, store, args)),
    };
    let (mut job, start) = match prepared {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(out, "sentier run: {}: {e}", pin.name);
            return EXIT_FATAL;
        }
    };
    let _ = writeln!(out, "log: {}", job.log);
    let cell = RefCell::new((&mut job, out));
    start(&CancelToken::default(), &|ev| {
        if let Ok(mut c) = cell.try_borrow_mut() {
            let (job, out) = &mut *c;
            narrate(job, &ev, &mut **out);
            job.apply(ev, Instant::now());
        }
    });
    let (job, out) = cell.into_inner();
    let report = job.report(Instant::now());
    keep(&report);
    summary(&report, out);
    exit_code(&report)
}

type Start = Box<dyn FnOnce(&CancelToken, &dyn Fn(JobEvent))>;

fn new_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn prepare_sync(preset: SyncPreset, args: &RunArgs, out: &mut dyn Write) -> (Job, Start) {
    let mode = if preset.mode == SyncMode::Mirror && !args.allow_mirror {
        let _ = writeln!(
            out,
            "note: {} is a mirror preset; running it as update (nothing local is deleted). \
             --allow-mirror runs it as mirror.",
            preset.name
        );
        SyncMode::Update
    } else {
        preset.mode
    };
    let _ = writeln!(
        out,
        "sync {} ({}): {} → {}",
        preset.name,
        mode.word(),
        preset.source.display(),
        preset.dest.display()
    );
    let id = new_id();
    let plan = Plan {
        backend: Backend::Robocopy,
        steps: vec![preset.step()],
        pairs: Vec::new(),
        title: preset.name.clone(),
    };
    let log = super::log_path(id, &plan.title).unwrap_or_else(|| format!("sentier-job-{id}.log"));
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
        threads: args.threads,
        log,
        mirror: mode == SyncMode::Mirror,
        exclude_dirs: preset.exclude_dirs.clone(),
        exclude_files: preset.exclude_files.clone(),
        totals: None,
    };
    (
        job,
        Box::new(move |cancel, send| super::run::run(&spec, cancel, send)),
    )
}

fn prepare_backup(
    preset: BackupPreset,
    store: &PinStore,
    args: &RunArgs,
) -> Result<(Job, Start), String> {
    let zip_name = preset.zip_name(chrono::Local::now())?;
    let pattern = preset.pattern()?;
    // A sync preset's local copy is zipped directly, as in the TUI.
    let direct = store
        .pins
        .iter()
        .filter(|p| p.kind == PinKind::Sync)
        .filter_map(|p| SyncPreset::from_pin(p).ok())
        .any(|s| s.dest.starts_with(&preset.source) && preset.source.starts_with(&s.dest));
    let id = new_id();
    let staging = if direct {
        None
    } else {
        Some(super::zip::staging_dir(id).ok_or("no %LOCALAPPDATA% to stage in")?)
    };
    let log = super::log_path(id, &preset.name).unwrap_or_else(|| format!("sentier-job-{id}.log"));
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
        zip_name,
        pattern,
        keep: preset.keep,
        compression: preset.compression,
        engine: preset.engine,
        threads: args.threads,
        log,
    };
    Ok((
        job,
        Box::new(move |cancel, send| super::zip::run(&spec, cancel, send)),
    ))
}

/// A line per milestone (not per file): a scheduled task's output is read after the fact.
fn narrate(job: &Job, ev: &JobEvent, out: &mut dyn Write) {
    let line = match ev {
        JobEvent::Scanned { files, bytes } => {
            format!("  {files} files, {} to copy", human_bytes(*bytes))
        }
        JobEvent::Stage(BackupStep::Staging) if job.backup.as_ref().is_some_and(|b| b.direct) => {
            "  staging: skipped (the source is a sync preset's local copy)".into()
        }
        JobEvent::Stage(s) => format!("  {} {}/{}…", s.word(), s.number(), BackupStep::ALL.len()),
        JobEvent::Produced {
            path,
            bytes,
            entries,
        } => format!(
            "  zip verified: {path} ({}, {entries} entries)",
            human_bytes(*bytes)
        ),
        JobEvent::Pruned(names) if !names.is_empty() => {
            format!("  retention removed {}", names.join(", "))
        }
        JobEvent::Error(e) => format!("  ERROR {e}"),
        _ => return,
    };
    let _ = writeln!(out, "{line}");
}

/// The report for the TUI's jobs panel and the preset's last run.
fn keep(r: &Report) {
    if let Some(dir) = super::jobs_dir()
        && let Err(e) = super::save_report(&dir, r)
    {
        tracing::warn!(error = %e, "saving a job report");
    }
    if let (Some(p), Some(path)) = (&r.preset, super::preset::runs_path()) {
        let mut runs = super::preset::load_runs(&path);
        runs.insert(p.key.clone(), LastRun::from_report(r));
        if let Err(e) = super::preset::save_runs(&path, &runs) {
            tracing::warn!(error = %e, "saving presets.json");
        }
    }
}

fn summary(r: &Report, out: &mut dyn Write) {
    let _ = writeln!(out, "{}", r.result_line());
    match &r.backup {
        Some(b) => {
            let _ = writeln!(out, "zip: {} ({})", b.zip, human_bytes(b.bytes));
        }
        None => {
            let _ = writeln!(
                out,
                "copied {} ({}) · skipped {} · failed {} · extra {}",
                r.copied,
                human_bytes(r.bytes),
                r.skipped,
                r.failed.len(),
                r.extra
            );
        }
    }
    for f in &r.failed {
        let _ = writeln!(out, "  failed: {}  ERROR {} {}", f.path, f.code, f.message);
    }
    let _ = writeln!(out, "took {}", clock(Duration::from_secs(r.seconds)));
}

/// robocopy-style: the run's own bits when it worked, 8 (or its failure bits) when not.
pub fn exit_code(r: &Report) -> i32 {
    match r.phase {
        Phase::Done if r.backup.is_some() => 1,
        Phase::Done => r.exit.unwrap_or(0).clamp(0, 7),
        _ => r.exit.filter(|c| *c >= 8).unwrap_or(EXIT_FAILED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn args_table() {
        assert_eq!(
            parse_args(&s(&["HIT → local", "--allow-mirror", "--threads", "8"])),
            Ok(Some(RunArgs {
                name: Some("HIT → local".into()),
                allow_mirror: true,
                threads: 8,
            }))
        );
        assert_eq!(parse_args(&s(&[])), Ok(Some(RunArgs::default())));
        assert_eq!(parse_args(&s(&["--help"])), Ok(None));
        for (a, why) in [
            (&["a", "b"][..], "one preset name"),
            (&["--threads", "0"][..], "--threads"),
            (&["--mirror"][..], "unknown option"),
        ] {
            let r = parse_args(&s(a));
            assert!(r.as_ref().is_err_and(|e| e.contains(why)), "{a:?}: {r:?}");
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sentier-headless-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("src").join("sub"));
        let _ = std::fs::create_dir_all(dir.join("dst"));
        let _ = std::fs::write(dir.join("src").join("a.txt"), "a");
        let _ = std::fs::write(dir.join("src").join("sub").join("b.txt"), "bb");
        let _ = std::fs::write(dir.join("dst").join("local-only.txt"), "x");
        dir
    }

    fn store(dir: &std::path::Path) -> PinStore {
        let text = format!(
            "[[pin]]\nkind = 'sync'\nname = 'Proj → local'\nsource = '{src}'\ndest = '{dst}'\n\
             mode = 'mirror'\n\n\
             [[pin]]\nkind = 'backup'\nname = 'Proj backup'\nsource = '{src}'\n\
             dest_dir = '{zips}'\nkeep = 1\n",
            src = dir.join("src").display(),
            dst = dir.join("dst").display(),
            zips = dir.join("zips").display(),
        );
        match crate::pins::parse(&text) {
            Ok((store, notes)) if notes.is_empty() => store,
            other => panic!("{other:?}"),
        }
    }

    fn go(store: &PinStore, args: &[&str]) -> (i32, String) {
        let Ok(Some(a)) = parse_args(&s(args)) else {
            panic!()
        };
        let mut out = Vec::new();
        let code = run(store, &a, &mut out);
        (code, String::from_utf8_lossy(&out).to_string())
    }

    #[test]
    fn sync_runs_as_update_unless_mirror_is_allowed() {
        let dir = temp("sync");
        let st = store(&dir);
        let (code, out) = go(&st, &["proj → LOCAL"]);
        // 1 (copied) + 2 (extra files in dest): still a success.
        assert_eq!(code, 3, "{out}");
        assert!(out.contains("running it as update"), "{out}");
        assert!(out.contains("DONE"), "{out}");
        assert!(dir.join("dst").join("sub").join("b.txt").exists());
        assert!(
            dir.join("dst").join("local-only.txt").exists(),
            "update deletes nothing"
        );

        let (code, out) = go(&st, &["Proj → local", "--allow-mirror"]);
        assert_eq!(code, 2, "nothing to copy, extras removed: {out}");
        assert!(
            !dir.join("dst").join("local-only.txt").exists(),
            "mirror deletes"
        );

        // The report is kept for the TUI, and the last run is recorded.
        let key = SyncPreset::from_pin(&st.pins[0])
            .map(|p| p.key())
            .unwrap_or_default();
        let runs = super::super::preset::runs_path()
            .map(|p| super::super::preset::load_runs(&p))
            .unwrap_or_default();
        assert_eq!(runs.get(&key).map(|r| r.phase), Some(Phase::Done));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_makes_a_zip_and_unknown_names_list_presets() {
        let dir = temp("backup");
        let st = store(&dir);
        let (code, out) = go(&st, &["proj backup"]);
        assert_eq!(code, 1, "{out}");
        assert!(
            out.contains("zipping 2/5") && out.contains("zip verified"),
            "{out}"
        );
        let zips = std::fs::read_dir(dir.join("zips")).map_or(0, |r| r.count());
        assert_eq!(zips, 1);

        let (code, out) = go(&st, &["proj"]);
        assert_eq!(code, EXIT_FATAL, "names match exactly, never by part");
        assert!(out.contains("no sync or backup preset named") && out.contains("Proj backup"));
        let (code, out) = go(&st, &[]);
        assert_eq!(code, EXIT_FATAL);
        assert!(out.contains("presets:"), "{out}");

        // A missing source: robocopy-style failure, nothing left behind.
        let _ = std::fs::remove_dir_all(dir.join("src"));
        let (code, out) = go(&st, &["Proj backup"]);
        assert!(code >= 8, "{code}: {out}");
        assert!(out.contains("FAILED"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
