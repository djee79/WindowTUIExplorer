//! Backup presets (CLAUDE.md section 9): a `kind = "backup"` pin zips a project folder
//! into a dated archive.
//!
//! The pipeline, one job, five steps shown in the jobs panel:
//! 1. **staging**: robocopy the source into `%LOCALAPPDATA%\sentier\staging\<job>`, so the
//!    zip is built from a stable local snapshot, not thousands of SMB reads. Skipped when
//!    the source is already a local copy (a sync preset's `dest`).
//! 2. **zipping** into `<dest_dir>\<file_name>.partial`;
//! 3. **verifying**: reopen it, check the entry count, read every entry (CRC-checked),
//!    then rename `.partial` to the final name, so a half-written zip never looks like a
//!    valid backup;
//! 4. **retention**: delete the oldest zips matching this preset's name pattern beyond
//!    `keep`; files that don't match are never touched;
//! 5. **cleanup**: delete the staging folder.
//!
//! Free space for staging + zip is checked before anything is written. Any failure or
//! a cancel removes the `.partial` and the staging folder.
//!
//! Everything above [`run`] is pure.

use super::preset::{PresetRef, check_paths};
use super::robocopy::{Overwrite, Step};
use super::run::{Spec, log_line, run_robocopy};
use super::{Backend, JobEvent, Plan, human_bytes};
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::CancelToken;
use crate::pins::{Pin, PinKind};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const DEFAULT_FILE_NAME: &str = "{name}_{date:%Y-%m-%d_%H%M}.zip";
const DEFAULT_DATE: &str = "%Y-%m-%d_%H%M";
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Free space kept spare on top of the estimate.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Store,
    Fast,
    Default,
    Best,
}

impl Compression {
    fn parse(s: &str) -> Result<Compression, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "store" => Ok(Compression::Store),
            "fast" => Ok(Compression::Fast),
            "default" => Ok(Compression::Default),
            "best" => Ok(Compression::Best),
            other => Err(format!(
                "compression {other:?}: store, fast, default or best"
            )),
        }
    }

    fn options(self) -> ::zip::write::SimpleFileOptions {
        use ::zip::CompressionMethod;
        let o = ::zip::write::SimpleFileOptions::default();
        match self {
            Compression::Store => o.compression_method(CompressionMethod::Stored),
            Compression::Fast => o
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(1)),
            Compression::Default => o.compression_method(CompressionMethod::Deflated),
            Compression::Best => o
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(9)),
        }
    }
}

impl Compression {
    /// 7-Zip's `-mx` level.
    fn mx(self) -> u8 {
        match self {
            Compression::Store => 0,
            Compression::Fast => 1,
            Compression::Default => 5,
            Compression::Best => 9,
        }
    }
}

/// What writes the zip. The built-in writer is the default and always works; 7-Zip is
/// faster (multi-threaded) when it's installed, and falls back to built-in when not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    Builtin,
    SevenZip,
}

impl Engine {
    pub fn parse(s: &str) -> Result<Engine, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "builtin" | "built-in" | "" => Ok(Engine::Builtin),
            "7zip" | "7-zip" | "7z" => Ok(Engine::SevenZip),
            other => Err(format!("engine {other:?}: builtin or 7zip")),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Engine::Builtin => "builtin",
            Engine::SevenZip => "7zip",
        }
    }
}

/// `7z.exe` on PATH, else in Program Files. Blocking (file checks): worker only.
pub fn find_7z() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    let installed = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|d| PathBuf::from(d).join("7-Zip"));
    on_path
        .into_iter()
        .chain(installed)
        .map(|d| d.join("7z.exe"))
        .find(|p| p.is_file())
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackupPreset {
    pub name: String,
    pub source: FsPath,
    pub dest_dir: FsPath,
    /// The template: `{name}_{date:%Y-%m-%d_%H%M}.zip`.
    pub file_name: String,
    /// Keep the newest N zips of this preset; 0 keeps all.
    pub keep: u32,
    pub compression: Compression,
    pub engine: Engine,
}

fn fs_path(field: &str, text: &str) -> Result<FsPath, String> {
    let text = crate::pins::expand_home(text).ok_or_else(|| format!("{field}: no home folder"))?;
    match Loc::parse(&text) {
        Ok(Loc::Fs(p)) => Ok(p),
        Ok(_) => Err(format!("{field} must be a folder path: {text}")),
        Err(e) => Err(format!("{field}: {e}")),
    }
}

impl BackupPreset {
    pub fn from_pin(pin: &Pin) -> Result<BackupPreset, String> {
        if pin.kind != PinKind::Backup {
            return Err(format!("{} is not a backup preset", pin.name));
        }
        let get = |k: &str| pin.extra.get(k).and_then(|v| v.as_str());
        let source = fs_path(
            "source",
            get("source").ok_or("backup preset needs a source")?,
        )?;
        let dest_dir = fs_path(
            "dest_dir",
            get("dest_dir").ok_or("backup preset needs a dest_dir")?,
        )?;
        // The zips may sit next to the project (source inside dest_dir), never in it.
        if let Err(e) = check_paths(&source, &dest_dir)
            && !e.contains("source is inside dest")
        {
            return Err(e.replace("dest", "dest_dir"));
        }
        let keep = match pin.extra.get("keep") {
            None => 0,
            Some(toml::Value::Integer(n)) if (0..=10_000).contains(n) => *n as u32,
            Some(_) => return Err("keep must be a number (0 keeps all)".into()),
        };
        let compression = match get("compression") {
            None => Compression::Default,
            Some(c) => Compression::parse(c)?,
        };
        let engine = Engine::parse(get("engine").unwrap_or("builtin"))?;
        let file_name = get("file_name").unwrap_or(DEFAULT_FILE_NAME).to_string();
        let preset = BackupPreset {
            name: pin.name.clone(),
            source,
            dest_dir,
            file_name,
            keep,
            compression,
            engine,
        };
        // A bad template is refused now, not at 2 a.m.
        preset.zip_name(chrono::Local::now())?;
        Ok(preset)
    }

    pub fn key(&self) -> String {
        format!(
            "backup|{}|{}",
            self.source.display(),
            self.dest_dir.display()
        )
        .to_lowercase()
    }

    pub fn preset_ref(&self) -> PresetRef {
        PresetRef {
            key: self.key(),
            label: "backup".into(),
        }
    }

    /// The zip's name for a run at `now`.
    pub fn zip_name(&self, now: chrono::DateTime<chrono::Local>) -> Result<String, String> {
        expand_name(&self.file_name, &self.source.name(), now)
    }

    /// The glob every zip of this preset matches (for retention).
    pub fn pattern(&self) -> Result<String, String> {
        name_glob(&self.file_name, &self.source.name())
    }
}

enum Piece<'a> {
    Text(&'a str),
    Name,
    Date(&'a str),
}

fn pieces(template: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            out.push(Piece::Text(&rest[..open]));
        }
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| format!("file_name: unclosed {{ in {template:?}"))?
            + open;
        let inner = &rest[open + 1..close];
        out.push(match inner.split_once(':') {
            None if inner == "name" => Piece::Name,
            None if inner == "date" => Piece::Date(DEFAULT_DATE),
            Some(("date", fmt)) => {
                let bad = chrono::format::StrftimeItems::new(fmt)
                    .any(|i| matches!(i, chrono::format::Item::Error));
                if bad || fmt.is_empty() {
                    return Err(format!("file_name: bad date format {fmt:?}"));
                }
                Piece::Date(fmt)
            }
            _ => {
                return Err(format!(
                    "file_name: unknown {{{inner}}} ({{name}}, {{date:…}})"
                ));
            }
        });
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    Ok(out)
}

fn with_zip(mut s: String) -> String {
    if !s.to_ascii_lowercase().ends_with(".zip") {
        s.push_str(".zip");
    }
    s
}

/// `{name}_{date:%Y-%m-%d_%H%M}.zip` → `HIT_2026-11-05_1945.zip`. `.zip` is added when
/// missing; the result must be a valid file name.
pub fn expand_name(
    template: &str,
    name: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Result<String, String> {
    let mut s = String::new();
    for p in pieces(template)? {
        match p {
            Piece::Text(t) => s.push_str(t),
            Piece::Name => s.push_str(name),
            Piece::Date(fmt) => {
                use std::fmt::Write as _;
                write!(s, "{}", now.format(fmt))
                    .map_err(|_| format!("file_name: bad date format {fmt:?}"))?;
            }
        }
    }
    let s = with_zip(s);
    crate::fs::ops::check_name(&s, false).map_err(|e| format!("file_name {s:?}: {e}"))?;
    Ok(s)
}

/// `{name}_{date:…}.zip` → `HIT_*.zip`: what retention may delete.
pub fn name_glob(template: &str, name: &str) -> Result<String, String> {
    let mut s = String::new();
    for p in pieces(template)? {
        match p {
            Piece::Text(t) => s.push_str(&globset::escape(t)),
            Piece::Name => s.push_str(&globset::escape(name)),
            Piece::Date(_) => s.push('*'),
        }
    }
    Ok(with_zip(s))
}

/// The files in `dest_dir` (name, modified) that retention deletes: those matching
/// `pattern`, beyond the `keep` newest. `keep` 0 deletes nothing.
pub fn prune(files: &[(String, SystemTime)], pattern: &str, keep: u32) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    let Ok(glob) = globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
    else {
        return Vec::new();
    };
    let m = glob.compile_matcher();
    let mut ours: Vec<&(String, SystemTime)> =
        files.iter().filter(|(n, _)| m.is_match(n)).collect();
    ours.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    ours.into_iter()
        .skip(keep as usize)
        .map(|(n, _)| n.clone())
        .collect()
}

/// Is there room? `staging` and `dest` are (volume, free bytes); staging needs the
/// source's size, the zip at most the same (stored, not compressed).
pub fn space_problem(
    bytes: u64,
    staging: Option<(&str, u64)>,
    dest: (&str, u64),
) -> Option<String> {
    let need = bytes + SPACE_MARGIN;
    match staging {
        Some((vol, free)) if vol.eq_ignore_ascii_case(dest.0) => (free < 2 * need).then(|| {
            format!(
                "not enough space on {vol}: staging + zip need about {}, {} free",
                human_bytes(2 * need),
                human_bytes(free)
            )
        }),
        Some((vol, free)) if free < need => Some(format!(
            "not enough space on {vol} for staging: about {} needed, {} free",
            human_bytes(need),
            human_bytes(free)
        )),
        _ => (dest.1 < need).then(|| {
            format!(
                "not enough space on {} for the zip: up to {} needed, {} free",
                dest.0,
                human_bytes(need),
                human_bytes(dest.1)
            )
        }),
    }
}

// ------------------------------------------------------------------ steps

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupStep {
    Staging,
    Zipping,
    Verifying,
    Retention,
    Cleanup,
}

impl BackupStep {
    pub const ALL: [BackupStep; 5] = [
        BackupStep::Staging,
        BackupStep::Zipping,
        BackupStep::Verifying,
        BackupStep::Retention,
        BackupStep::Cleanup,
    ];

    /// 1-based, for `ZIPPING 2/5`.
    pub fn number(self) -> usize {
        BackupStep::ALL
            .iter()
            .position(|s| *s == self)
            .map_or(1, |i| i + 1)
    }

    pub fn word(self) -> &'static str {
        match self {
            BackupStep::Staging => "staging",
            BackupStep::Zipping => "zipping",
            BackupStep::Verifying => "verifying",
            BackupStep::Retention => "retention",
            BackupStep::Cleanup => "cleanup",
        }
    }
}

/// A backup job's own progress, next to the robocopy tracker (used while staging).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupState {
    pub step: Option<BackupStep>,
    /// Bytes of the current step (zipped, verified).
    pub done: u64,
    pub total: u64,
    /// The file being zipped / verified.
    pub current: Option<String>,
    /// Staging was skipped (the source is already local).
    pub direct: bool,
    pub outcome: Option<BackupOutcome>,
    pub pruned: Vec<String>,
}

/// What a finished backup made, kept in the report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupOutcome {
    pub zip: String,
    pub bytes: u64,
    pub entries: u64,
    #[serde(default)]
    pub pruned: Vec<String>,
}

// ------------------------------------------------------------------ the run

pub struct BackupSpec {
    pub title: String,
    pub source: String,
    /// Where to stage; None zips the source directly.
    pub staging: Option<String>,
    pub dest_dir: String,
    /// The final name (already expanded).
    pub zip_name: String,
    /// Retention glob and count.
    pub pattern: String,
    pub keep: u32,
    pub compression: Compression,
    pub engine: Engine,
    /// robocopy `/MT:n` for staging.
    pub threads: u32,
    pub log: String,
}

/// `%LOCALAPPDATA%\sentier\staging\<job id>`.
pub fn staging_dir(id: u64) -> Option<String> {
    let base = if cfg!(test) {
        std::env::temp_dir().join("sentier-test-staging")
    } else {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("staging")
    };
    Some(base.join(id.to_string()).display().to_string())
}

/// `C:\x` → `\\?\C:\x`, `\\srv\s\x` → `\\?\UNC\srv\s\x`: no 260-character limit.
fn long(p: &str) -> PathBuf {
    if p.starts_with(r"\\?\") {
        PathBuf::from(p)
    } else if let Some(rest) = p.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{rest}"))
    } else {
        PathBuf::from(format!(r"\\?\{p}"))
    }
}

struct Found {
    /// `/`-separated, relative to the root; folders end in `/`.
    rel: String,
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// Every folder and file below `root`, folders before their contents, sorted by name.
/// Junctions and symlinks are skipped, never followed.
fn walk(root: &Path, cancel: &CancelToken) -> std::io::Result<Vec<Found>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        if cancel.is_cancelled() {
            break;
        }
        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&dir)?.flatten().collect();
        entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
        let mut subdirs = Vec::new();
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let meta = e.metadata().ok();
            let modified = meta.as_ref().and_then(|m| m.modified().ok());
            if ft.is_dir() {
                let rel = format!("{prefix}{name}/");
                out.push(Found {
                    rel: rel.clone(),
                    path: e.path(),
                    size: 0,
                    modified,
                });
                subdirs.push((e.path(), rel));
            } else {
                out.push(Found {
                    rel: format!("{prefix}{name}"),
                    path: e.path(),
                    size: meta.map_or(0, |m| m.len()),
                    modified,
                });
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(out)
}

fn zip_time(t: Option<SystemTime>) -> ::zip::DateTime {
    use chrono::{Datelike, Timelike};
    let Some(t) = t else {
        return ::zip::DateTime::default_for_write();
    };
    let l: chrono::DateTime<chrono::Local> = t.into();
    ::zip::DateTime::from_date_and_time(
        l.year().clamp(1980, 2107) as u16,
        l.month() as u8,
        l.day() as u8,
        l.hour() as u8,
        l.minute() as u8,
        l.second() as u8,
    )
    .unwrap_or_else(|_| ::zip::DateTime::default_for_write())
}

/// Runs the whole pipeline, then sends `Finished`.
pub fn run(spec: &BackupSpec, ctl: &super::Control, send: &dyn Fn(JobEvent)) {
    let cancel = &ctl.cancel;
    log_line(&spec.log, &format!("sentier backup: {}", spec.title));
    let partial = Path::new(&spec.dest_dir).join(format!("{}.partial", spec.zip_name));
    let result = pipeline(spec, &partial, ctl, send);
    // Whatever happened, no half-written zip and no staging folder stay behind.
    if result.is_err() || cancel.is_cancelled() {
        let _ = std::fs::remove_file(long(&partial.display().to_string()));
        if let Some(dir) = &spec.staging {
            send(JobEvent::Stage(BackupStep::Cleanup));
            if let Err(e) = crate::fs::ops::remove(&long(dir))
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log_line(&spec.log, &format!("staging not removed: {dir}: {e}"));
            }
        }
    }
    let cancelled = cancel.is_cancelled();
    match result {
        Err(e) if !cancelled => {
            log_line(&spec.log, &format!("FAILED: {e}"));
            send(JobEvent::Error(e));
        }
        _ => log_line(&spec.log, if cancelled { "cancelled" } else { "finished" }),
    }
    send(JobEvent::Finished { cancelled });
}

fn pipeline(
    spec: &BackupSpec,
    partial: &Path,
    ctl: &super::Control,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    let cancel = &ctl.cancel;
    let stop = || cancel.is_cancelled().then(|| "cancelled".to_string());
    std::fs::create_dir_all(long(&spec.dest_dir))
        .map_err(|e| format!("can't create {}: {e}", spec.dest_dir))?;

    // 1. Staging (or, for a local source, just its size).
    send(JobEvent::Stage(BackupStep::Staging));
    let root = match &spec.staging {
        Some(staging) => {
            stage(spec, staging, ctl, send)?;
            staging.clone()
        }
        None => {
            let found = walk(&long(&spec.source), cancel)
                .map_err(|e| format!("can't read {}: {e}", spec.source))?;
            let files = found.iter().filter(|f| !f.rel.ends_with('/'));
            let bytes = files.clone().map(|f| f.size).sum();
            check_space(spec, bytes)?;
            send(JobEvent::Scanned {
                files: files.count() as u64,
                bytes,
            });
            spec.source.clone()
        }
    };
    if let Some(e) = stop() {
        return Err(e);
    }

    // 2. Zipping.
    send(JobEvent::Stage(BackupStep::Zipping));
    let found = walk(&long(&root), cancel).map_err(|e| format!("can't read {root}: {e}"))?;
    let total: u64 = found.iter().map(|f| f.size).sum();
    let files = found.iter().filter(|f| !f.rel.ends_with('/')).count() as u64;
    let seven = match spec.engine {
        Engine::SevenZip => {
            let exe = find_7z();
            if exe.is_none() {
                log_line(&spec.log, "7-Zip not found: using the built-in zip writer");
            }
            exe
        }
        Engine::Builtin => None,
    };
    match &seven {
        Some(exe) => zip_7z(exe, spec, partial, &root, total, cancel, send)?,
        None => write_zip(spec, partial, &found, total, cancel, send)?,
    }
    if let Some(e) = stop() {
        return Err(e);
    }

    // 3. Verifying, then the final name.
    send(JobEvent::Stage(BackupStep::Verifying));
    let entries = verify(partial, files, total, cancel, send)?;
    if let Some(e) = stop() {
        return Err(e);
    }
    let fin = free_name(Path::new(&spec.dest_dir), &spec.zip_name);
    std::fs::rename(
        long(&partial.display().to_string()),
        long(&fin.display().to_string()),
    )
    .map_err(|e| format!("can't rename the zip to {}: {e}", fin.display()))?;
    let bytes = std::fs::metadata(long(&fin.display().to_string())).map_or(0, |m| m.len());
    log_line(
        &spec.log,
        &format!(
            "zip ok: {} ({}, {entries} entries)",
            fin.display(),
            human_bytes(bytes)
        ),
    );
    send(JobEvent::Produced {
        path: fin.display().to_string(),
        bytes,
        entries,
    });

    // 4. Retention: never fails the backup, which is already safe.
    send(JobEvent::Stage(BackupStep::Retention));
    let listed: Vec<(String, SystemTime)> = std::fs::read_dir(long(&spec.dest_dir))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| {
                    let m = e.metadata().ok()?.modified().ok()?;
                    Some((e.file_name().to_string_lossy().to_string(), m))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut pruned = Vec::new();
    for name in prune(&listed, &spec.pattern, spec.keep) {
        let p = Path::new(&spec.dest_dir).join(&name);
        match std::fs::remove_file(long(&p.display().to_string())) {
            Ok(()) => {
                log_line(&spec.log, &format!("retention: deleted {name}"));
                pruned.push(name);
            }
            Err(e) => log_line(&spec.log, &format!("retention: {name} not deleted: {e}")),
        }
    }
    send(JobEvent::Pruned(pruned));

    // 5. Cleanup.
    send(JobEvent::Stage(BackupStep::Cleanup));
    if let Some(dir) = &spec.staging
        && let Err(e) = crate::fs::ops::remove(&long(dir))
    {
        log_line(&spec.log, &format!("staging not removed: {dir}: {e}"));
    }
    Ok(())
}

/// `HIT_2026-11-05_1945.zip`, or `…_2.zip` if that is taken (two runs in a minute).
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !long(&first.display().to_string()).exists() {
        return first;
    }
    let stem = name.strip_suffix(".zip").unwrap_or(name);
    (2..1000)
        .map(|n| dir.join(format!("{stem}_{n}.zip")))
        .find(|p| !long(&p.display().to_string()).exists())
        .unwrap_or(first)
}

fn volume(path: &str) -> String {
    match Loc::parse(path) {
        Ok(Loc::Fs(p)) => p.volume_root().display(),
        _ => path.to_string(),
    }
}

fn check_space(spec: &BackupSpec, bytes: u64) -> Result<(), String> {
    let free = |p: &str| crate::platform::disk_space(&volume(p)).map(|s| s.free).ok();
    let Some(dest_free) = free(&spec.dest_dir) else {
        return Ok(()); // can't tell: the copy itself will say
    };
    let staging = spec
        .staging
        .as_deref()
        .and_then(|s| Some((volume(s), free(s)?)));
    match space_problem(
        bytes,
        staging.as_ref().map(|(v, f)| (v.as_str(), *f)),
        (&volume(&spec.dest_dir), dest_free),
    ) {
        Some(p) => Err(p),
        None => Ok(()),
    }
}

/// robocopy the source into the staging folder; its `/L` pass gives the size checked
/// against the free space.
fn stage(
    spec: &BackupSpec,
    staging: &str,
    ctl: &super::Control,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    let cancel = &ctl.cancel;
    let step = Step {
        src: spec.source.clone(),
        dst: staging.to_string(),
        files: Vec::new(),
    };
    let opts = super::robocopy::Options {
        moving: false,
        overwrite: Overwrite::Changed,
        mirror: false,
        list_only: true,
        threads: 0,
        log: None,
        exclude_dirs: Vec::new(),
        exclude_files: Vec::new(),
    };
    let (t, code) = super::run::dry_run(&step, &opts, cancel)?;
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    if !(0..8).contains(&code) {
        let why = t
            .errors
            .first()
            .map(|e| format!("ERROR {} {}: {}", e.code, e.action, e.message))
            .unwrap_or_else(|| super::robocopy::exit_meaning(code).parts);
        return Err(format!("can't read {}: {why}", spec.source));
    }
    let (files, bytes) = t
        .summary
        .map_or((0, 0), |s| (s.files.copied, s.bytes.copied));
    check_space(spec, bytes)?;
    let worst = std::cell::Cell::new(0i32);
    let errored = std::cell::Cell::new(None::<String>);
    let rspec = Spec {
        plan: Plan {
            backend: Backend::Robocopy,
            steps: vec![step],
            pairs: Vec::new(),
            title: spec.title.clone(),
        },
        moving: false,
        overwrite: Overwrite::Changed,
        threads: spec.threads,
        log: spec.log.clone(),
        mirror: false,
        exclude_dirs: Vec::new(),
        exclude_files: Vec::new(),
        totals: Some((files, bytes)),
    };
    let cancelled = run_robocopy(&rspec, ctl, &|ev| {
        match &ev {
            JobEvent::StepDone(c) => {
                worst.set(worst.get() | *c.max(&0) | if *c < 0 { 16 } else { 0 })
            }
            JobEvent::Error(e) => errored.set(Some(e.clone())),
            _ => {}
        }
        send(ev);
    });
    if cancelled {
        return Err("cancelled".into());
    }
    if let Some(e) = errored.take() {
        return Err(e);
    }
    if worst.get() >= 8 {
        return Err(format!(
            "staging failed ({}): no zip was made",
            super::robocopy::exit_meaning(worst.get()).parts
        ));
    }
    Ok(())
}

/// Sends `StageProgress` at most every 100 ms.
struct Progress<'a> {
    send: &'a dyn Fn(JobEvent),
    total: u64,
    done: u64,
    last: Instant,
}

impl Progress<'_> {
    fn tick(&mut self, current: &str, force: bool) {
        if force || self.last.elapsed() >= PROGRESS_EVERY {
            self.last = Instant::now();
            (self.send)(JobEvent::StageProgress {
                done: self.done,
                total: self.total,
                current: Some(current.to_string()),
            });
        }
    }
}

fn write_zip(
    spec: &BackupSpec,
    partial: &Path,
    found: &[Found],
    total: u64,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    let err = |what: &str, e: &dyn std::fmt::Display| format!("zipping {what}: {e}");
    let file = std::fs::File::create(long(&partial.display().to_string()))
        .map_err(|e| err(&partial.display().to_string(), &e))?;
    let mut zw = ::zip::ZipWriter::new(std::io::BufWriter::with_capacity(1 << 20, file));
    let mut p = Progress {
        send,
        total,
        done: 0,
        last: Instant::now(),
    };
    let mut buf = vec![0u8; 1 << 20];
    for f in found {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let opts = spec
            .compression
            .options()
            .last_modified_time(zip_time(f.modified))
            .large_file(f.size >= u64::from(u32::MAX));
        if f.rel.ends_with('/') {
            zw.add_directory(f.rel.clone(), opts)
                .map_err(|e| err(&f.rel, &e))?;
            continue;
        }
        zw.start_file(f.rel.as_str(), opts)
            .map_err(|e| err(&f.rel, &e))?;
        let mut src = std::fs::File::open(&f.path).map_err(|e| err(&f.rel, &e))?;
        p.tick(&f.rel, true);
        loop {
            let n = src.read(&mut buf).map_err(|e| err(&f.rel, &e))?;
            if n == 0 {
                break;
            }
            zw.write_all(&buf[..n]).map_err(|e| err(&f.rel, &e))?;
            p.done += n as u64;
            p.tick(&f.rel, false);
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
        }
    }
    let mut out = zw.finish().map_err(|e| err("the archive", &e))?;
    out.flush().map_err(|e| err("the archive", &e))?;
    out.get_ref()
        .sync_all()
        .map_err(|e| err("the archive", &e))?;
    p.tick("", true);
    Ok(())
}

/// The zip written by 7-Zip (`7z a -tzip -mx=N -mmt=on`, run in `root` so names are
/// relative). Its `NN%` progress becomes `StageProgress`; its other output goes to the
/// job log. Any warning (a file it couldn't read) fails the backup: an incomplete zip
/// is no backup.
fn zip_7z(
    exe: &Path,
    spec: &BackupSpec,
    partial: &Path,
    root: &str,
    total: u64,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let args = [
        "a".to_string(),
        "-tzip".into(),
        format!("-mx={}", spec.compression.mx()),
        "-mmt=on".into(),
        "-bsp1".into(),
        "-bso1".into(),
        "-bse1".into(),
        "-y".into(),
        partial.display().to_string(),
        "*".into(),
    ];
    log_line(
        &spec.log,
        &format!("7-Zip: {} {} (in {root})", exe.display(), args.join(" ")),
    );
    let mut child = Command::new(exe)
        .args(&args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("7-Zip didn't start: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
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
    let mut p = Progress {
        send,
        total,
        done: 0,
        last: Instant::now(),
    };
    let mut text = String::new();
    let mut killed = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(bytes) => {
                text.push_str(&crate::platform::oem_to_string(&bytes));
                // Lines end in \r, \n or runs of backspaces (progress redraws).
                while let Some(i) = text.find(['\r', '\n', '\u{8}']) {
                    let line: String = text.drain(..=i).collect();
                    let line = line.trim_matches(|c: char| c.is_whitespace() || c == '\u{8}');
                    if let Some(pct) = percent(line) {
                        p.done = total * pct / 100;
                        p.tick(line, false);
                    } else if !line.is_empty() {
                        log_line(&spec.log, &format!("7-Zip: {line}"));
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if cancel.is_cancelled() && !killed {
            let _ = child.kill();
            killed = true;
        }
    }
    let _ = reader.join();
    let status = child.wait().map_err(|e| e.to_string())?;
    if killed {
        return Err("cancelled".into());
    }
    match status.code() {
        Some(0) => {
            p.done = total;
            p.tick("", true);
            Ok(())
        }
        Some(1) => {
            Err("7-Zip couldn't read some files (warnings in the log): no zip was made".into())
        }
        Some(c) => Err(format!("7-Zip failed (exit {c}, see the log)")),
        None => Err("7-Zip didn't exit normally".into()),
    }
}

/// `45% 12 + hitmac\x.pmlmac` → 45.
fn percent(line: &str) -> Option<u64> {
    let (n, _) = line.split_once('%')?;
    n.trim().parse::<u64>().ok().filter(|n| *n <= 100)
}

/// Every entry read back in full: the zip crate checks each CRC as it reaches the end.
/// The zip must hold exactly `files` files. Returns its entry count (files + folders).
fn verify(
    partial: &Path,
    files: u64,
    total: u64,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<u64, String> {
    let bad = |e: &dyn std::fmt::Display| format!("verify failed: {e}");
    let file = std::fs::File::open(long(&partial.display().to_string())).map_err(|e| bad(&e))?;
    let mut za = ::zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| bad(&e))?;
    let in_zip = (0..za.len())
        .filter(|&i| za.by_index_raw(i).is_ok_and(|f| !f.is_dir()))
        .count() as u64;
    if in_zip != files {
        return Err(format!(
            "verify failed: the zip has {in_zip} files, the source {files}"
        ));
    }
    let mut p = Progress {
        send,
        total,
        done: 0,
        last: Instant::now(),
    };
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..za.len() {
        let mut f = za.by_index(i).map_err(|e| bad(&e))?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().map(|n| n.to_string()).unwrap_or_default();
        p.tick(&name, true);
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| format!("verify failed: {name}: {e}"))?;
            if n == 0 {
                break;
            }
            p.done += n as u64;
            p.tick(&name, false);
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
        }
    }
    Ok(za.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{Job, Op, Phase};
    use chrono::TimeZone;
    use std::sync::Mutex;

    fn pin(text: &str) -> Pin {
        match toml::from_str::<Pin>(text) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        }
    }

    fn at(d: u32, h: u32, m: u32) -> chrono::DateTime<chrono::Local> {
        match chrono::Local
            .with_ymd_and_hms(2026, 11, d, h, m, 0)
            .single()
        {
            Some(t) => t,
            None => panic!(),
        }
    }

    #[test]
    fn preset_from_pin_and_names() {
        let pr = BackupPreset::from_pin(&pin(r#"
            kind = "backup"
            name = "HIT backup"
            source = '\\srv01\project\HIT'
            dest_dir = 'E:\backups\aveva'
            keep = 10
            compression = "best"
        "#));
        let Ok(pr) = pr else { panic!("{pr:?}") };
        assert_eq!((pr.keep, pr.compression), (10, Compression::Best));
        assert_eq!(pr.file_name, DEFAULT_FILE_NAME);
        assert_eq!(
            pr.zip_name(at(5, 19, 45)),
            Ok("HIT_2026-11-05_1945.zip".into())
        );
        assert_eq!(pr.pattern(), Ok("HIT_*.zip".into()));
        assert_eq!(pr.key(), r"backup|\\srv01\project\hit|e:\backups\aveva");

        let table = [
            ("{name}-{date}", "HIT-2026-11-05_1945.zip"),
            ("{name}_{date:%Y%m%d}.ZIP", "HIT_20261105.ZIP"),
            ("nightly", "nightly.zip"),
        ];
        for (t, want) in table {
            assert_eq!(expand_name(t, "HIT", at(5, 19, 45)), Ok(want.into()), "{t}");
        }
        for (t, why) in [
            ("{name", "unclosed"),
            ("{who}", "unknown"),
            ("{date:%Q}", "bad date format"),
            ("{date:%Y/%m}", "file_name"),
        ] {
            let r = expand_name(t, "HIT", at(5, 19, 45));
            assert!(r.as_ref().is_err_and(|e| e.contains(why)), "{t}: {r:?}");
        }
        assert_eq!(
            name_glob("[x] {name}_{date}", "a*b"),
            Ok(r"[[]x[]] a[*]b_*.zip".into())
        );

        for (text, why) in [
            ("kind='backup'\nname='x'\nsource='C:\\a'", "dest_dir"),
            (
                "kind='backup'\nname='x'\nsource='C:\\a'\ndest_dir='C:\\a\\zips'",
                "inside source",
            ),
            (
                "kind='backup'\nname='x'\nsource='C:\\a'\ndest_dir='D:\\z'\nkeep=-1",
                "keep",
            ),
            (
                "kind='backup'\nname='x'\nsource='C:\\a'\ndest_dir='D:\\z'\ncompression='max'",
                "compression",
            ),
            (
                "kind='backup'\nname='x'\nsource='C:\\a'\ndest_dir='D:\\z'\nfile_name='{oops}'",
                "unknown",
            ),
        ] {
            let r = BackupPreset::from_pin(&pin(text));
            assert!(r.as_ref().is_err_and(|e| e.contains(why)), "{text}: {r:?}");
        }
        // Zips next to the project (source inside dest_dir) are fine.
        let r = BackupPreset::from_pin(&pin(
            "kind='backup'\nname='x'\nsource='D:\\aveva\\HIT'\ndest_dir='D:\\aveva'",
        ));
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn retention_keeps_the_newest_of_ours_only() {
        let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        let files = vec![
            ("HIT_2026-11-01_0800.zip".to_string(), t(1)),
            ("HIT_2026-11-02_0800.zip".to_string(), t(2)),
            ("hit_2026-11-03_0800.ZIP".to_string(), t(3)),
            ("HIT_2026-11-04_0800.zip".to_string(), t(4)),
            ("HIT_2026-11-04_0800.zip.partial".to_string(), t(9)),
            ("OTHER_2026-11-01.zip".to_string(), t(0)),
            ("notes.txt".to_string(), t(0)),
        ];
        assert_eq!(
            prune(&files, "HIT_*.zip", 2),
            ["HIT_2026-11-02_0800.zip", "HIT_2026-11-01_0800.zip"]
        );
        assert!(prune(&files, "HIT_*.zip", 0).is_empty(), "0 keeps all");
        assert!(prune(&files, "HIT_*.zip", 10).is_empty());
    }

    #[test]
    fn space_table() {
        const G: u64 = 1 << 30;
        assert_eq!(
            space_problem(G, Some(("C:\\", 10 * G)), ("E:\\", 10 * G)),
            None
        );
        assert!(
            space_problem(G, Some(("C:\\", G / 2)), ("E:\\", 10 * G))
                .is_some_and(|e| e.contains("staging"))
        );
        assert!(
            space_problem(G, Some(("C:\\", 10 * G)), ("E:\\", G / 2))
                .is_some_and(|e| e.contains("for the zip"))
        );
        // Same volume: room for both.
        assert!(
            space_problem(3 * G, Some(("C:\\", 5 * G)), ("c:\\", 5 * G))
                .is_some_and(|e| e.contains("staging + zip"))
        );
        assert_eq!(space_problem(G, None, ("E:\\", 2 * G)), None);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sentier-zip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn drive(spec: &BackupSpec) -> Job {
        let events = Mutex::new(Vec::new());
        run(spec, &crate::jobs::Control::default(), &|e| {
            if let Ok(mut v) = events.lock() {
                v.push(e);
            }
        });
        let plan = Plan {
            backend: Backend::Robocopy,
            steps: vec![],
            pairs: vec![],
            title: spec.title.clone(),
        };
        let mut job = Job::new(
            1,
            &plan,
            Op::Copy,
            spec.dest_dir.clone(),
            Overwrite::Changed,
            spec.log.clone(),
        );
        job.backup = Some(BackupState {
            direct: spec.staging.is_none(),
            ..BackupState::default()
        });
        for e in events.into_inner().unwrap_or_default() {
            job.apply(e, Instant::now());
        }
        job
    }

    fn read_zip(path: &str) -> Vec<String> {
        let Ok(f) = std::fs::File::open(path) else {
            panic!("{path}")
        };
        let Ok(mut za) = ::zip::ZipArchive::new(f) else {
            panic!()
        };
        (0..za.len())
            .filter_map(|i| {
                za.by_index(i)
                    .ok()
                    .and_then(|f| f.name().ok().map(|n| n.to_string()))
            })
            .collect()
    }

    #[test]
    fn backup_stages_zips_verifies_and_prunes() {
        let dir = temp("run");
        let src = dir.join("HIT");
        let _ = std::fs::create_dir_all(src.join("hitmac").join("empty"));
        let _ = std::fs::write(src.join("evars.bat"), "set x=1");
        let _ = std::fs::write(src.join("hitmac").join("é.pmlmac"), "é".repeat(5000));
        let dest = dir.join("zips");
        let _ = std::fs::create_dir_all(&dest);
        // Two old backups of ours and a stranger's.
        for n in [
            "HIT_2026-01-01_0000.zip",
            "HIT_2026-01-02_0000.zip",
            "OTHER.zip",
        ] {
            let _ = std::fs::write(dest.join(n), "old");
        }
        let staging = dir.join("staging").join("1");
        let spec = BackupSpec {
            title: "HIT backup".into(),
            source: src.display().to_string(),
            staging: Some(staging.display().to_string()),
            dest_dir: dest.display().to_string(),
            zip_name: "HIT_2026-11-06_0900.zip".into(),
            pattern: "HIT_*.zip".into(),
            keep: 2,
            compression: Compression::Default,
            engine: Engine::Builtin,
            threads: 0,
            log: dir.join("job.log").display().to_string(),
        };
        let job = drive(&spec);
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        let zip = dest.join("HIT_2026-11-06_0900.zip");
        let names = read_zip(&zip.display().to_string());
        assert_eq!(
            names,
            ["evars.bat", "hitmac/", "hitmac/empty/", "hitmac/é.pmlmac"]
        );
        let b = job.backup.clone().unwrap_or_default();
        let out = b.outcome.unwrap_or_default();
        assert_eq!(out.entries, 4);
        assert_eq!(
            b.pruned,
            ["HIT_2026-01-01_0000.zip"],
            "keep 2: the new one and the newest old one"
        );
        assert!(dest.join("HIT_2026-01-02_0000.zip").exists());
        assert!(dest.join("OTHER.zip").exists(), "not ours: never touched");
        assert!(!staging.exists(), "staging is cleaned up");
        assert!(!dest.join("HIT_2026-11-06_0900.zip.partial").exists());
        assert_eq!(job.total_files, Some(2));

        // Again in the same minute, straight from the source: `_2`.
        let direct = BackupSpec {
            staging: None,
            keep: 0,
            ..spec
        };
        let job = drive(&direct);
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        assert!(job.backup.is_some_and(|b| b.direct));
        assert_eq!(
            read_zip(&dest.join("HIT_2026-11-06_0900_2.zip").display().to_string()).len(),
            4
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_catches_a_corrupted_entry() {
        let dir = temp("corrupt");
        let zip = dir.join("t.zip");
        let mut spec = BackupSpec {
            title: "t".into(),
            source: dir.display().to_string(),
            staging: None,
            dest_dir: dir.display().to_string(),
            zip_name: "t.zip".into(),
            pattern: "t*.zip".into(),
            keep: 0,
            compression: Compression::Store,
            engine: Engine::Builtin,
            threads: 0,
            log: dir.join("job.log").display().to_string(),
        };
        let data = dir.join("data");
        let _ = std::fs::create_dir_all(&data);
        let _ = std::fs::write(data.join("a.txt"), "MARKER-0123456789");
        spec.source = data.display().to_string();
        let found = walk(&long(&spec.source), &CancelToken::default()).unwrap_or_default();
        let total = found.iter().map(|f| f.size).sum();
        let ok = write_zip(&spec, &zip, &found, total, &CancelToken::default(), &|_| {});
        assert_eq!(ok, Ok(()));
        assert_eq!(
            verify(&zip, 1, total, &CancelToken::default(), &|_| {}),
            Ok(1)
        );
        assert!(
            verify(&zip, 2, total, &CancelToken::default(), &|_| {})
                .is_err_and(|e| e.contains("the zip has 1 files, the source 2"))
        );
        // Flip one byte of the stored data: the CRC no longer matches.
        let mut bytes = std::fs::read(&zip).unwrap_or_default();
        let Some(at) = bytes.windows(6).position(|w| w == b"MARKER") else {
            panic!("stored data not found")
        };
        bytes[at] = b'X';
        let _ = std::fs::write(&zip, bytes);
        let r = verify(&zip, 1, total, &CancelToken::default(), &|_| {});
        assert!(
            r.as_ref()
                .is_err_and(|e| e.contains("verify failed: a.txt")),
            "{r:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engines_and_7zip_progress() {
        assert_eq!(Engine::parse("7-Zip"), Ok(Engine::SevenZip));
        assert_eq!(Engine::parse("builtin"), Ok(Engine::Builtin));
        assert!(Engine::parse("rar").is_err_and(|e| e.contains("builtin or 7zip")));
        let r = BackupPreset::from_pin(&pin(
            "kind='backup'\nname='x'\nsource='C:\\a'\ndest_dir='D:\\z'\nengine='7zip'",
        ));
        assert_eq!(r.map(|p| p.engine), Ok(Engine::SevenZip));
        assert_eq!(percent("45% 12 + hitmac\\x.pmlmac"), Some(45));
        assert_eq!(percent("  0%"), Some(0));
        assert_eq!(percent("Everything is Ok"), None);
        assert_eq!(percent("250% nope"), None);
        assert_eq!((Compression::Store.mx(), Compression::Best.mx()), (0, 9));
    }

    /// The same pipeline through 7-Zip, when it's installed (skipped otherwise).
    #[test]
    fn backup_through_7zip_when_installed() {
        if find_7z().is_none() {
            eprintln!("7-Zip not installed: skipped");
            return;
        }
        let dir = temp("7z");
        let src = dir.join("HIT");
        let _ = std::fs::create_dir_all(src.join("hitmac").join("empty"));
        let _ = std::fs::write(src.join("evars.bat"), "set x=1");
        let _ = std::fs::write(src.join("hitmac").join("é.pmlmac"), "é".repeat(5000));
        let dest = dir.join("zips");
        let spec = BackupSpec {
            title: "HIT backup".into(),
            source: src.display().to_string(),
            staging: None,
            dest_dir: dest.display().to_string(),
            zip_name: "HIT_7z.zip".into(),
            pattern: "HIT_*.zip".into(),
            keep: 0,
            compression: Compression::Best,
            engine: Engine::SevenZip,
            threads: 0,
            log: dir.join("job.log").display().to_string(),
        };
        let job = drive(&spec);
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        let mut names = read_zip(&dest.join("HIT_7z.zip").display().to_string());
        names.sort();
        let files: Vec<&String> = names.iter().filter(|n| !n.ends_with('/')).collect();
        assert_eq!(files, ["evars.bat", "hitmac/é.pmlmac"], "{names:?}");
        assert!(
            names
                .iter()
                .any(|n| n.trim_end_matches('/') == "hitmac/empty"),
            "{names:?}"
        );
        let log = std::fs::read_to_string(dir.join("job.log")).unwrap_or_default();
        assert!(log.contains("7-Zip:"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_source_fails_and_leaves_nothing() {
        let dir = temp("missing");
        let dest = dir.join("zips");
        let spec = BackupSpec {
            title: "t".into(),
            source: dir.join("nope").display().to_string(),
            staging: Some(dir.join("staging").display().to_string()),
            dest_dir: dest.display().to_string(),
            zip_name: "t.zip".into(),
            pattern: "t*.zip".into(),
            keep: 1,
            compression: Compression::Store,
            engine: Engine::Builtin,
            threads: 0,
            log: dir.join("job.log").display().to_string(),
        };
        let job = drive(&spec);
        assert_eq!(job.phase, Phase::Failed);
        assert!(
            job.error.as_ref().is_some_and(|e| e.contains("can't read")),
            "{:?}",
            job.error
        );
        let left: Vec<_> = std::fs::read_dir(&dest)
            .map(|r| r.flatten().count())
            .into_iter()
            .collect();
        assert_eq!(left, [0], "no zip, no partial");
        assert!(!dir.join("staging").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
