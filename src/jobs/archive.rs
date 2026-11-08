//! Compress and extract (`<leader>zc` / `<leader>zx`, `:zip`, `:unzip`, the `O` menu):
//! jobs in the panel like a copy, with progress, cancel and a report.
//!
//! - **Compress** zips the chosen files and folders with the backup's zip writer, then
//!   reads every entry back (CRC-checked) like a backup does.
//! - **Extract** unpacks a `.zip` itself, every CRC checked on the way; an entry whose
//!   name would land outside the target folder (`..\..\x`) refuses the whole archive.
//!   Other formats (`.7z`, `.rar`, `.tar.gz`…) go through 7-Zip when it's installed.
//!   The files go into a folder named after the archive (`HIT.zip` → `HIT\`), or, when
//!   the archive holds one top folder, that folder lands here as it is (no `HIT\HIT\`).
//!   A name already taken gets a number (`HIT_1`), as paste does.
//!
//! A zip is written as `<name>.partial` and renamed when verified. A `.zip` extracts
//! straight into its final (new) folder, decided up front from the entry names: no
//! folder rename at the end, which Windows refuses while antivirus or the indexer still
//! has one of the new files open. 7-Zip formats go through `<name>.partial` and a rename
//! that is tried again for a while. A failure or a cancel removes what was made.
//!
//! Everything above [`run`] is pure.

use super::run::log_line;
use super::zip::{
    BackupStep, Compression, Found, Progress, free_name, long, percent, verify, walk, write_zip,
};
use super::{Control, JobEvent, human_bytes};
use crate::fs::worker::CancelToken;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What reads an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// sentier itself.
    Zip,
    /// 7-Zip (`7z.exe`), when installed.
    SevenZip,
}

/// Extensions 7-Zip unpacks (besides `.zip`). Double ones (`.tar.gz`) end in one of these.
const SEVEN_ZIP_EXTS: &[&str] = &[
    "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "cab",
];

/// `HIT.zip` → Zip, `x.tar.gz` → SevenZip, `notes.txt` → None.
pub fn format_of(name: &str) -> Option<Format> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if ext == "zip" {
        Some(Format::Zip)
    } else if SEVEN_ZIP_EXTS.contains(&ext.as_str()) {
        Some(Format::SevenZip)
    } else {
        None
    }
}

/// The folder an archive extracts into: `HIT.zip` → `HIT`, `src.tar.gz` → `src`.
pub fn archive_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for double in [".tar.gz", ".tar.bz2", ".tar.xz"] {
        if lower.ends_with(double) {
            return name[..name.len() - double.len()].to_string();
        }
    }
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name.to_string(),
    }
}

/// The suggested zip name: one file → its stem (`report.pdf` → `report.zip`, as
/// Windows' "Compressed folder" does), one folder → its name, several → the folder
/// they're in.
pub fn suggested_zip(items: &[(String, bool)], folder: &str) -> String {
    let base = match items {
        [(name, true)] => name.clone(),
        [(name, false)] => archive_stem(name),
        _ => folder.to_string(),
    };
    let base = base.trim_end_matches(['\\', ':']);
    format!("{}.zip", if base.is_empty() { "archive" } else { base })
}

/// The zip prompt's answer: `.zip` added when missing, and a valid file name.
pub fn zip_file_name(text: &str) -> Result<String, String> {
    let t = text.trim();
    let name = if t.to_ascii_lowercase().ends_with(".zip") {
        t.to_string()
    } else {
        format!("{t}.zip")
    };
    crate::fs::ops::check_name(&name, false)?;
    if name.len() <= 4 {
        return Err("a name, please".into());
    }
    Ok(name)
}

/// `name`, or `name_1`, `name_2`… when `taken` (lowercased) has it.
pub fn free_folder(name: &str, taken: &HashSet<String>) -> String {
    if taken.contains(&name.to_lowercase()) {
        super::free_name(name, true, taken)
    } else {
        name.to_string()
    }
}

// ------------------------------------------------------------------ the target

/// Where an archive's files go, named by the archive: its one top folder (`aveva.zip`
/// holding `aveva\…` → `aveva`), else the archive's own name. Read before extracting
/// (on the blocking pool), so an existing folder can be asked about first.
pub fn target_folder(archive: &str, seven: Option<&Path>) -> Result<String, String> {
    let name = name_of(archive);
    let entries = match format_of(name) {
        Some(Format::Zip) => zip_entries(archive)?,
        Some(Format::SevenZip) => match seven {
            Some(exe) => list_7z(exe, archive)?,
            None => return Err("this kind of archive needs 7-Zip (7z.exe), not found".into()),
        },
        None => return Err(format!("{name} isn't an archive sentier knows")),
    };
    Ok(top_folder(&entries).unwrap_or_else(|| archive_stem(name)))
}

/// A `.zip`'s entries (path, is a folder); an entry that would land outside the target
/// folder refuses the archive.
fn zip_entries(archive: &str) -> Result<Vec<(PathBuf, bool)>, String> {
    let bad = |e: &dyn std::fmt::Display| format!("{}: {e}", name_of(archive));
    let file = std::fs::File::open(long(archive)).map_err(|e| bad(&e))?;
    let mut za = ::zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| bad(&e))?;
    let mut out = Vec::with_capacity(za.len());
    for i in 0..za.len() {
        let f = za.by_index_raw(i).map_err(|e| bad(&e))?;
        let Some(rel) = f.enclosed_name() else {
            return Err(format!(
                "{}: the entry {:?} would land outside the folder; nothing extracted",
                name_of(archive),
                f.name()
            ));
        };
        out.push((rel, f.is_dir()));
    }
    Ok(out)
}

/// `7z l -ba -slt`: each entry's `Path = …` and `Folder = +`.
fn list_7z(exe: &Path, archive: &str) -> Result<Vec<(PathBuf, bool)>, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(exe)
        .args(["l", "-ba", "-slt", archive])
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("7-Zip didn't start: {e}"))?;
    if !out.status.success() {
        return Err(format!("7-Zip can't read {}", name_of(archive)));
    }
    Ok(parse_7z_list(&crate::platform::oem_to_string(&out.stdout)))
}

/// The entries in `7z l -slt` output.
pub fn parse_7z_list(text: &str) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("Path = ") {
            out.push((PathBuf::from(p.trim()), false));
        } else if let Some(f) = line.strip_prefix("Folder = ")
            && let Some(last) = out.last_mut()
        {
            last.1 = f.trim() == "+";
        }
    }
    out
}

// ------------------------------------------------------------------ the run

/// Extract into a new folder, or into one that's there already (asked first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractTo {
    /// A new folder by this name (`_1` if it got taken meanwhile).
    New(String),
    /// Into this existing folder: files with the same name are replaced, nothing else
    /// is touched, and the folder is never removed (not even on a failure).
    Merge(String),
}

/// A compress or extract job, run on the blocking pool.
#[derive(Debug, Clone)]
pub enum ArchiveJob {
    Compress {
        /// The files and folders, full paths.
        items: Vec<String>,
        dest_dir: String,
        /// The final name (`.zip`).
        zip_name: String,
        log: String,
    },
    Extract {
        archive: String,
        dest_dir: String,
        to: ExtractTo,
        /// 7-Zip, for anything but a `.zip`.
        seven: Option<PathBuf>,
        log: String,
    },
}

impl ArchiveJob {
    /// For the confirmation and the audit log: `zip 3 items into HIT.zip`.
    pub fn describe(&self) -> String {
        match self {
            ArchiveJob::Compress {
                items, zip_name, ..
            } => match items.as_slice() {
                [one] => format!("zip {} into {zip_name}", name_of(one)),
                _ => format!("zip {} items into {zip_name}", items.len()),
            },
            ArchiveJob::Extract { archive, to, .. } => match to {
                ExtractTo::New(f) => format!("extract {} into {f}", name_of(archive)),
                ExtractTo::Merge(f) => {
                    format!("extract {} into the existing {f}", name_of(archive))
                }
            },
        }
    }

    /// The paths it touches (for the audit log).
    pub fn paths(&self) -> Vec<String> {
        match self {
            ArchiveJob::Compress {
                items,
                dest_dir,
                zip_name,
                ..
            } => {
                let mut v = items.clone();
                v.push(format!(
                    "-> {}\\{zip_name}",
                    dest_dir.trim_end_matches('\\')
                ));
                v
            }
            ArchiveJob::Extract {
                archive, dest_dir, ..
            } => vec![archive.clone(), format!("-> {dest_dir}")],
        }
    }

    fn log(&self) -> &str {
        match self {
            ArchiveJob::Compress { log, .. } | ArchiveJob::Extract { log, .. } => log,
        }
    }
}

fn name_of(path: &str) -> &str {
    path.trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(path)
}

/// Runs the job, then sends `Finished`. Whatever happens, no `.partial` stays behind.
pub fn run(job: &ArchiveJob, ctl: &Control, send: &dyn Fn(JobEvent)) {
    let log = job.log();
    log_line(log, &format!("sentier: {}", job.describe()));
    let mut partial: Option<PathBuf> = None;
    let result = match job {
        ArchiveJob::Compress {
            items,
            dest_dir,
            zip_name,
            ..
        } => compress(items, dest_dir, zip_name, &mut partial, &ctl.cancel, send),
        ArchiveJob::Extract {
            archive,
            dest_dir,
            to,
            seven,
            ..
        } => extract(
            archive,
            dest_dir,
            to,
            seven.as_deref(),
            log,
            &mut partial,
            &ctl.cancel,
            send,
        ),
    };
    let cancelled = ctl.cancel.is_cancelled();
    if (result.is_err() || cancelled)
        && let Some(p) = &partial
        && let Err(e) = crate::fs::ops::remove(&long(&p.display().to_string()))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log_line(log, &format!("not removed: {}: {e}", p.display()));
    }
    match result {
        Err(e) if !cancelled => {
            log_line(log, &format!("FAILED: {e}"));
            send(JobEvent::Error(e));
        }
        _ => log_line(log, if cancelled { "cancelled" } else { "finished" }),
    }
    send(JobEvent::Finished { cancelled });
}

fn compress(
    items: &[String],
    dest_dir: &str,
    zip_name: &str,
    partial: &mut Option<PathBuf>,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    send(JobEvent::Stage(BackupStep::Zipping));
    let mut found = Vec::new();
    for item in items {
        let path = long(item);
        let meta = std::fs::metadata(&path).map_err(|e| format!("can't read {item}: {e}"))?;
        let name = name_of(item).to_string();
        let modified = meta.modified().ok();
        if meta.is_dir() {
            found.push(Found {
                rel: format!("{name}/"),
                path: path.clone(),
                size: 0,
                modified,
            });
            let inside = walk(&path, cancel).map_err(|e| format!("can't read {item}: {e}"))?;
            found.extend(inside.into_iter().map(|f| Found {
                rel: format!("{name}/{}", f.rel),
                ..f
            }));
        } else {
            found.push(Found {
                rel: name,
                path,
                size: meta.len(),
                modified,
            });
        }
    }
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let total: u64 = found.iter().map(|f| f.size).sum();
    let files = found.iter().filter(|f| !f.rel.ends_with('/')).count() as u64;
    let dir = Path::new(dest_dir);
    let part = dir.join(format!("{zip_name}.partial"));
    *partial = Some(part.clone());
    write_zip(Compression::Default, &part, &found, total, cancel, send)?;
    send(JobEvent::Stage(BackupStep::Verifying));
    let entries = verify(&part, files, total, cancel, send)?;
    let fin = free_name(dir, zip_name);
    rename_retry(&part, &fin, cancel)
        .map_err(|e| rename_failed(&format!("can't rename the zip to {}", fin.display()), &e))?;
    *partial = None;
    let bytes = std::fs::metadata(long(&fin.display().to_string())).map_or(0, |m| m.len());
    send(JobEvent::Produced {
        path: fin.display().to_string(),
        bytes,
        entries,
    });
    Ok(())
}

/// The names already in `dir`, lowercased (for `HIT_1`).
fn taken_in(dir: &str) -> HashSet<String> {
    std::fs::read_dir(long(dir))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

/// Renames `from` to `to`, trying again for ~10 s while Windows says "access denied" or
/// "in use": right after thousands of files were written, antivirus, the search
/// indexer or a sync client often still has some open, and a folder holding an open
/// file can't be renamed.
fn rename_retry(from: &Path, to: &Path, cancel: &CancelToken) -> std::io::Result<()> {
    let (from, to) = (
        long(&from.display().to_string()),
        long(&to.display().to_string()),
    );
    let mut tries = 0;
    loop {
        match std::fs::rename(&from, &to) {
            Err(e)
                if matches!(e.raw_os_error(), Some(5 | 32 | 33))
                    && tries < 25
                    && !cancel.is_cancelled() =>
            {
                tries += 1;
                std::thread::sleep(Duration::from_millis(400));
            }
            r => return r,
        }
    }
}

/// Why a final rename failed, in words.
fn rename_failed(what: &str, e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(5 | 32 | 33) => format!(
            "{what}: {e}. Something still has files in it open (antivirus, the search \
             indexer, OneDrive?); try again in a moment"
        ),
        _ => format!("{what}: {e}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn extract(
    archive: &str,
    dest_dir: &str,
    to: &ExtractTo,
    seven: Option<&Path>,
    log: &str,
    partial: &mut Option<PathBuf>,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    send(JobEvent::Stage(BackupStep::Extracting));
    let name = name_of(archive);
    let taken = taken_in(dest_dir);
    // The folder: new (numbered if it got taken since it was asked), or the existing one.
    let fin = match to {
        ExtractTo::New(f) => Path::new(dest_dir).join(free_folder(f, &taken)),
        ExtractTo::Merge(f) => Path::new(dest_dir).join(f),
    };
    let merge = matches!(to, ExtractTo::Merge(_));
    let (entries, bytes) = match format_of(name) {
        Some(Format::Zip) => unzip(archive, &fin, merge, partial, cancel, send)?,
        Some(Format::SevenZip) => {
            let exe = seven.ok_or("this kind of archive needs 7-Zip (7z.exe), not found")?;
            extract_7z(exe, archive, &fin, merge, taken, log, partial, cancel, send)?
        }
        None => return Err(format!("{name} isn't an archive sentier knows")),
    };
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    *partial = None;
    log_line(
        log,
        &format!(
            "extracted to {} ({entries} entries, {})",
            fin.display(),
            human_bytes(bytes)
        ),
    );
    send(JobEvent::Produced {
        path: fin.display().to_string(),
        bytes,
        entries,
    });
    Ok(())
}

/// The one folder every entry is in (`HIT`, `HIT/a.txt` → `HIT`), if there is one.
/// A file at the top (`a.txt`) means there is none. Entries: (path, is a folder).
pub fn top_folder(entries: &[(PathBuf, bool)]) -> Option<String> {
    let mut top: Option<String> = None;
    for (path, is_dir) in entries {
        let mut parts = path.components();
        let first = parts.next()?.as_os_str().to_string_lossy().to_string();
        if parts.next().is_none() && !is_dir {
            return None;
        }
        match &top {
            None => top = Some(first),
            Some(t) if *t == first => {}
            Some(_) => return None,
        }
    }
    top
}

/// A `.zip` straight into `fin`: (entries, bytes). Its one top folder, if any, is `fin`
/// itself (no `HIT\HIT`). Nothing is renamed at the end: Windows refuses to rename a
/// folder while antivirus or the indexer has one of the new files open. A new `fin` is
/// removed on a failure or a cancel (`partial`); a merged-into one never is. Reading
/// each entry to its end checks its CRC.
fn unzip(
    archive: &str,
    fin: &Path,
    merge: bool,
    partial: &mut Option<PathBuf>,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(u64, u64), String> {
    let bad = |e: &dyn std::fmt::Display| format!("{}: {e}", name_of(archive));
    // Every name checked before anything is written.
    let names = zip_entries(archive)?;
    let file = std::fs::File::open(long(archive)).map_err(|e| bad(&e))?;
    let mut za = ::zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| bad(&e))?;
    let mut total = 0u64;
    for i in 0..za.len() {
        total += za.by_index_raw(i).map_err(|e| bad(&e))?.size();
    }
    let top = top_folder(&names);
    let fin_s = fin.display().to_string();
    if merge {
        std::fs::create_dir_all(long(&fin_s)).map_err(|e| format!("can't open {fin_s}: {e}"))?;
    } else {
        // `create_dir`, not `_all`: it must be new, since a failure deletes it.
        std::fs::create_dir(long(&fin_s)).map_err(|e| format!("can't create {fin_s}: {e}"))?;
        *partial = Some(fin.to_path_buf());
    }
    let mut p = Progress {
        send,
        total,
        done: 0,
        last: Instant::now(),
    };
    let mut buf = vec![0u8; 1 << 20];
    let root = fin.display().to_string();
    for i in 0..za.len() {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let mut f = za.by_index(i).map_err(|e| bad(&e))?;
        let Some(rel) = f.enclosed_name() else {
            continue;
        };
        // The top folder is `fin` itself.
        let rel = match &top {
            Some(t) => match rel.strip_prefix(t) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            },
            None => rel,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel = rel.display().to_string().replace('/', "\\");
        let target = long(&format!("{root}\\{rel}"));
        if f.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| format!("{rel}: {e}"))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{rel}: {e}"))?;
        }
        let mut out = std::fs::File::create(&target).map_err(|e| format!("{rel}: {e}"))?;
        p.tick(&rel, true);
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| format!("{rel}: {e} (damaged archive?)"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(|e| format!("{rel}: {e}"))?;
            p.done += n as u64;
            p.tick(&rel, false);
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
        }
        if let Some(t) = f.last_modified().and_then(system_time) {
            let _ = out.set_modified(t);
        }
    }
    p.tick("", true);
    Ok((za.len() as u64, total))
}

/// Anything 7-Zip reads: unpacked into `<name>.partial` next to `fin`, then into `fin`
/// (a single folder at the top of what came out is `fin` itself): renamed to it when
/// new, its files moved in when merging. Moves are tried again while files are busy.
#[allow(clippy::too_many_arguments)]
fn extract_7z(
    exe: &Path,
    archive: &str,
    fin: &Path,
    merge: bool,
    taken: HashSet<String>,
    log: &str,
    partial: &mut Option<PathBuf>,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(u64, u64), String> {
    let name = name_of(archive);
    let dir = fin.parent().unwrap_or(fin);
    let part = dir.join(free_folder(
        &format!("{}.partial", archive_stem(name)),
        &taken,
    ));
    std::fs::create_dir(long(&part.display().to_string()))
        .map_err(|e| format!("can't create {}: {e}", part.display()))?;
    *partial = Some(part.clone());
    let (entries, bytes) = unpack_7z(exe, archive, &part, log, cancel, send)?;
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let inside: Vec<std::fs::DirEntry> = std::fs::read_dir(long(&part.display().to_string()))
        .map_err(|e| format!("can't read {}: {e}", part.display()))?
        .flatten()
        .collect();
    let src = match inside.as_slice() {
        [one] if one.file_type().is_ok_and(|t| t.is_dir()) => part.join(one.file_name()),
        _ => part.clone(),
    };
    if merge {
        merge_into(&src, fin, cancel)?;
        let _ = crate::fs::ops::remove(&long(&part.display().to_string()));
        *partial = None;
    } else {
        rename_retry(&src, fin, cancel)
            .map_err(|e| rename_failed(&format!("can't move it to {}", fin.display()), &e))?;
        if src != part {
            let _ = std::fs::remove_dir(long(&part.display().to_string()));
        }
    }
    Ok((entries, bytes))
}

/// Every file under `src` moved into the same place under `dst`, replacing a file with
/// the same name; folders made as needed. Nothing else in `dst` is touched.
fn merge_into(src: &Path, dst: &Path, cancel: &CancelToken) -> Result<(), String> {
    let found = walk(&long(&src.display().to_string()), cancel)
        .map_err(|e| format!("can't read {}: {e}", src.display()))?;
    std::fs::create_dir_all(long(&dst.display().to_string()))
        .map_err(|e| format!("can't open {}: {e}", dst.display()))?;
    for f in found {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let rel = f.rel.trim_end_matches('/').replace('/', "\\");
        let to = dst.join(&rel);
        if f.rel.ends_with('/') {
            std::fs::create_dir_all(long(&to.display().to_string()))
                .map_err(|e| format!("{rel}: {e}"))?;
        } else {
            rename_retry(&f.path, &to, cancel)
                .map_err(|e| rename_failed(&format!("can't move {rel} into place"), &e))?;
        }
    }
    Ok(())
}

/// A zip entry's local time.
fn system_time(t: ::zip::DateTime) -> Option<std::time::SystemTime> {
    use chrono::TimeZone;
    let local = chrono::Local
        .with_ymd_and_hms(
            i32::from(t.year()),
            u32::from(t.month()),
            u32::from(t.day()),
            u32::from(t.hour()),
            u32::from(t.minute()),
            u32::from(t.second()),
        )
        .earliest()?;
    Some(local.into())
}

/// Anything 7-Zip reads, into `into`: (files, bytes). A `.tar.gz` holds a `.tar`, which
/// is unpacked in turn.
fn unpack_7z(
    exe: &Path,
    archive: &str,
    into: &Path,
    log: &str,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(u64, u64), String> {
    let total = std::fs::metadata(long(archive)).map_or(0, |m| m.len());
    run_7z(exe, archive, into, total, log, cancel, send)?;
    let inner: Vec<PathBuf> = std::fs::read_dir(long(&into.display().to_string()))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    if let [tar] = inner.as_slice()
        && tar
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("tar"))
    {
        let tar_path = tar.display().to_string();
        let total = std::fs::metadata(tar).map_or(0, |m| m.len());
        run_7z(exe, &tar_path, into, total, log, cancel, send)?;
        std::fs::remove_file(tar).map_err(|e| format!("{tar_path}: {e}"))?;
    }
    let found = walk(&long(&into.display().to_string()), cancel)
        .map_err(|e| format!("can't read {}: {e}", into.display()))?;
    let files = found.iter().filter(|f| !f.rel.ends_with('/'));
    Ok((files.clone().count() as u64, files.map(|f| f.size).sum()))
}

/// `7z x -o<into> <archive>`; its `NN%` becomes progress, its other output the log.
fn run_7z(
    exe: &Path,
    archive: &str,
    into: &Path,
    total: u64,
    log: &str,
    cancel: &CancelToken,
    send: &dyn Fn(JobEvent),
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let args = [
        "x".to_string(),
        "-y".into(),
        "-bsp1".into(),
        "-bso1".into(),
        "-bse1".into(),
        format!("-o{}", into.display()),
        archive.to_string(),
    ];
    log_line(log, &format!("7-Zip: {} {}", exe.display(), args.join(" ")));
    let mut child = Command::new(exe)
        .args(&args)
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
                while let Some(i) = text.find(['\r', '\n', '\u{8}']) {
                    let line: String = text.drain(..=i).collect();
                    let line = line.trim_matches(|c: char| c.is_whitespace() || c == '\u{8}');
                    if let Some(pct) = percent(line) {
                        p.done = total * pct / 100;
                        p.tick(line, false);
                    } else if !line.is_empty() {
                        log_line(log, &format!("7-Zip: {line}"));
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
        Some(1) => Err("7-Zip reported problems with some files (see the log)".into()),
        Some(c) => Err(format!("7-Zip failed (exit {c}, see the log)")),
        None => Err("7-Zip didn't exit normally".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn formats_and_stems() {
        let cases: &[(&str, Option<Format>, &str)] = &[
            ("HIT.zip", Some(Format::Zip), "HIT"),
            ("HIT.ZIP", Some(Format::Zip), "HIT"),
            ("x.7z", Some(Format::SevenZip), "x"),
            ("src.tar.gz", Some(Format::SevenZip), "src"),
            ("src.TAR.XZ", Some(Format::SevenZip), "src"),
            ("a.tgz", Some(Format::SevenZip), "a"),
            ("setup.rar", Some(Format::SevenZip), "setup"),
            ("notes.txt", None, "notes"),
            ("README", None, "README"),
            (".zip", Some(Format::Zip), ".zip"),
        ];
        for (name, fmt, stem) in cases {
            assert_eq!(format_of(name), *fmt, "{name}");
            assert_eq!(archive_stem(name), *stem, "{name}");
        }
    }

    #[test]
    fn zip_names() {
        let it = |n: &str, d: bool| (n.to_string(), d);
        assert_eq!(
            suggested_zip(&[it("report.pdf", false)], "work"),
            "report.zip"
        );
        assert_eq!(suggested_zip(&[it("HIT.v2", true)], "work"), "HIT.v2.zip");
        assert_eq!(
            suggested_zip(&[it("a", false), it("b", true)], "work"),
            "work.zip"
        );
        assert_eq!(
            suggested_zip(&[it("a", false), it("b", true)], "C:"),
            "C.zip"
        );
        assert_eq!(zip_file_name(" HIT "), Ok("HIT.zip".into()));
        assert_eq!(zip_file_name("HIT.ZIP"), Ok("HIT.ZIP".into()));
        assert!(zip_file_name("").is_err());
        assert!(zip_file_name(".zip").is_err());
        assert!(zip_file_name("a/b").is_err());
        assert!(zip_file_name(r"sub\x").is_err());
    }

    #[test]
    fn top_folder_rule() {
        let v = |s: &[(&str, bool)]| -> Vec<(PathBuf, bool)> {
            s.iter().map(|(p, d)| (PathBuf::from(p), *d)).collect()
        };
        type Case<'a> = (&'a [(&'a str, bool)], Option<&'a str>);
        let cases: &[Case] = &[
            (
                &[("HIT", true), ("HIT/a.txt", false), ("HIT/sub/b", false)],
                Some("HIT"),
            ),
            (&[("HIT/a.txt", false)], Some("HIT")),
            (&[("HIT", true)], Some("HIT")),
            (&[("HIT/a.txt", false), ("b.txt", false)], None),
            (&[("a.txt", false)], None),
            (&[("A/x", false), ("B/y", false)], None),
            (&[], None),
        ];
        for (entries, want) in cases {
            assert_eq!(top_folder(&v(entries)).as_deref(), *want, "{entries:?}");
        }
    }

    #[test]
    fn a_damaged_zip_leaves_nothing_behind() {
        let dir = temp("crc");
        let s = |p: &Path| p.display().to_string();
        let zip = dir.join("bad.zip");
        let Ok(file) = std::fs::File::create(&zip) else {
            panic!()
        };
        let mut zw = ::zip::ZipWriter::new(file);
        let opts = ::zip::write::SimpleFileOptions::default()
            .compression_method(::zip::CompressionMethod::Stored);
        let ok = zw
            .start_file("good.txt", opts)
            .and_then(|()| zw.write_all(b"fine").map_err(Into::into))
            .and_then(|()| zw.start_file("bad.txt", opts))
            .and_then(|()| zw.write_all(b"MARKER-DATA").map_err(Into::into))
            .and_then(|()| zw.finish().map(|_| ()));
        assert!(ok.is_ok(), "{ok:?}");
        // Flip a byte of the stored data: its CRC no longer matches.
        let mut bytes = std::fs::read(&zip).unwrap_or_default();
        let at = bytes.windows(11).position(|w| w == b"MARKER-DATA");
        let Some(at) = at else {
            panic!("data not found")
        };
        bytes[at] = b'X';
        let _ = std::fs::write(&zip, bytes);
        let (_, r) = run_job(&ArchiveJob::Extract {
            archive: s(&zip),
            dest_dir: s(&dir),
            to: ExtractTo::New("bad".into()),
            seven: None,
            log: s(&dir.join("job.log")),
        });
        assert!(r.is_err(), "a CRC error fails the job");
        assert!(!dir.join("bad").exists(), "the half-made folder is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_7z_list_names_its_entries() {
        let text = "Path = aveva\nFolder = +\nSize = 0\n\nPath = aveva\\a.txt\nFolder = -\n";
        assert_eq!(
            parse_7z_list(text),
            [
                (PathBuf::from("aveva"), true),
                (PathBuf::from(r"aveva\a.txt"), false)
            ]
        );
        assert_eq!(top_folder(&parse_7z_list(text)).as_deref(), Some("aveva"));
    }

    /// `name.zip` in `dir` holding these (path, contents); a path ending in `/` is a folder.
    fn make_zip(dir: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
        let zip = dir.join(name);
        let Ok(file) = std::fs::File::create(&zip) else {
            panic!()
        };
        let mut zw = ::zip::ZipWriter::new(file);
        let opts = ::zip::write::SimpleFileOptions::default();
        for (path, body) in entries {
            let r = if path.ends_with('/') {
                zw.add_directory(*path, opts)
            } else {
                zw.start_file(*path, opts)
                    .and_then(|()| zw.write_all(body.as_bytes()).map_err(Into::into))
            };
            assert!(r.is_ok(), "{r:?}");
        }
        assert!(zw.finish().is_ok());
        zip
    }

    #[test]
    fn the_zip_names_its_folder() {
        let dir = temp("target");
        let s = |p: &Path| p.display().to_string();
        let one = make_zip(&dir, "export.zip", &[("aveva/", ""), ("aveva/a.txt", "a")]);
        assert_eq!(target_folder(&s(&one), None), Ok("aveva".into()));
        let loose = make_zip(&dir, "loose.zip", &[("a.txt", "a"), ("sub/b.txt", "b")]);
        assert_eq!(target_folder(&s(&loose), None), Ok("loose".into()));
        assert!(target_folder(&s(&dir.join("x.7z")), None).is_err_and(|e| e.contains("7-Zip")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_replaces_same_names_and_keeps_the_rest() {
        let dir = temp("merge");
        let s = |p: &Path| p.display().to_string();
        let _ = std::fs::create_dir_all(dir.join(r"aveva\sub"));
        let _ = std::fs::write(dir.join(r"aveva\a.txt"), "old");
        let _ = std::fs::write(dir.join(r"aveva\mine.txt"), "keep me");
        let zip = make_zip(
            &dir,
            "aveva.zip",
            &[("aveva/a.txt", "new"), ("aveva/sub/b.txt", "b")],
        );
        let (events, r) = run_job(&ArchiveJob::Extract {
            archive: s(&zip),
            dest_dir: s(&dir),
            to: ExtractTo::Merge("aveva".into()),
            seven: None,
            log: s(&dir.join("job.log")),
        });
        assert_eq!(r, Ok(()));
        assert_eq!(produced(&events), Some(s(&dir.join("aveva"))));
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).ok();
        assert_eq!(read(r"aveva\a.txt").as_deref(), Some("new"), "replaced");
        assert_eq!(
            read(r"aveva\mine.txt").as_deref(),
            Some("keep me"),
            "untouched"
        );
        assert_eq!(read(r"aveva\sub\b.txt").as_deref(), Some("b"));
        assert!(!dir.join("aveva_1").exists() && !dir.join(r"aveva\aveva").exists());

        // A merge that fails never removes the folder that was there.
        let _ = std::fs::write(dir.join("broken.zip"), "not a zip");
        let (_, r) = run_job(&ArchiveJob::Extract {
            archive: s(&dir.join("broken.zip")),
            dest_dir: s(&dir),
            to: ExtractTo::Merge("aveva".into()),
            seven: None,
            log: s(&dir.join("job.log")),
        });
        assert!(r.is_err());
        assert_eq!(read(r"aveva\mine.txt").as_deref(), Some("keep me"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_folder_numbers() {
        let taken: HashSet<String> = ["hit".to_string(), "hit_1".to_string()].into();
        assert_eq!(free_folder("HIT", &taken), "HIT_2");
        assert_eq!(free_folder("Other", &taken), "Other");
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sentier-archive-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn run_job(job: &ArchiveJob) -> (Vec<JobEvent>, Result<(), String>) {
        let events = Mutex::new(Vec::new());
        run(job, &Control::default(), &|ev| {
            if let Ok(mut v) = events.lock() {
                v.push(ev);
            }
        });
        let events = events.into_inner().unwrap_or_default();
        let err = events.iter().find_map(|e| match e {
            JobEvent::Error(e) => Some(e.clone()),
            _ => None,
        });
        (events, err.map_or(Ok(()), Err))
    }

    fn produced(events: &[JobEvent]) -> Option<String> {
        events.iter().find_map(|e| match e {
            JobEvent::Produced { path, .. } => Some(path.clone()),
            _ => None,
        })
    }

    #[test]
    fn compress_then_extract_round_trip() {
        let dir = temp("round");
        let s = |p: &Path| p.display().to_string();
        let _ = std::fs::create_dir_all(dir.join(r"HIT\sub"));
        let _ = std::fs::write(dir.join(r"HIT\a.txt"), "alpha");
        let _ = std::fs::write(dir.join(r"HIT\sub\b.txt"), "beta");
        let _ = std::fs::write(dir.join("notes.txt"), "notes");
        let log = s(&dir.join("job.log"));

        // A folder and a file into one zip.
        let (events, r) = run_job(&ArchiveJob::Compress {
            items: vec![s(&dir.join("HIT")), s(&dir.join("notes.txt"))],
            dest_dir: s(&dir),
            zip_name: "both.zip".into(),
            log: log.clone(),
        });
        assert_eq!(r, Ok(()));
        assert_eq!(produced(&events), Some(s(&dir.join("both.zip"))));
        assert!(!dir.join("both.zip.partial").exists());

        // Two things at the top: into a folder named after the zip.
        let (events, r) = run_job(&ArchiveJob::Extract {
            archive: s(&dir.join("both.zip")),
            dest_dir: s(&dir),
            to: ExtractTo::New("both".into()),
            seven: None,
            log: log.clone(),
        });
        assert_eq!(r, Ok(()));
        assert_eq!(produced(&events), Some(s(&dir.join("both"))));
        assert_eq!(
            std::fs::read_to_string(dir.join(r"both\HIT\sub\b.txt"))
                .ok()
                .as_deref(),
            Some("beta")
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(r"both\notes.txt"))
                .ok()
                .as_deref(),
            Some("notes")
        );

        // One folder at the top: it lands as it is, numbered since HIT exists.
        let (_, r) = run_job(&ArchiveJob::Compress {
            items: vec![s(&dir.join("HIT"))],
            dest_dir: s(&dir),
            zip_name: "HIT.zip".into(),
            log: log.clone(),
        });
        assert_eq!(r, Ok(()));
        let (events, r) = run_job(&ArchiveJob::Extract {
            archive: s(&dir.join("HIT.zip")),
            dest_dir: s(&dir),
            to: ExtractTo::New("HIT".into()),
            seven: None,
            log: log.clone(),
        });
        assert_eq!(r, Ok(()));
        assert_eq!(produced(&events), Some(s(&dir.join("HIT_1"))));
        assert_eq!(
            std::fs::read_to_string(dir.join(r"HIT_1\a.txt"))
                .ok()
                .as_deref(),
            Some("alpha")
        );
        assert!(!dir.join(r"HIT_1\HIT").exists(), "no HIT\\HIT");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".partial"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // The same zip name again: numbered, the first one untouched.
        let (events, _) = run_job(&ArchiveJob::Compress {
            items: vec![s(&dir.join("notes.txt"))],
            dest_dir: s(&dir),
            zip_name: "HIT.zip".into(),
            log,
        });
        assert_eq!(produced(&events), Some(s(&dir.join("HIT_2.zip"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_slip_entry_refuses_the_whole_archive() {
        let dir = temp("slip");
        let s = |p: &Path| p.display().to_string();
        let zip = dir.join("evil.zip");
        let Ok(file) = std::fs::File::create(&zip) else {
            panic!()
        };
        let mut zw = ::zip::ZipWriter::new(file);
        let opts = ::zip::write::SimpleFileOptions::default();
        let ok = zw
            .start_file("fine.txt", opts)
            .and_then(|()| zw.write_all(b"ok").map_err(Into::into))
            .and_then(|()| zw.start_file("../../escaped.txt", opts))
            .and_then(|()| zw.write_all(b"x").map_err(Into::into))
            .and_then(|()| zw.finish().map(|_| ()));
        assert!(ok.is_ok(), "{ok:?}");
        let (_, r) = run_job(&ArchiveJob::Extract {
            archive: s(&zip),
            dest_dir: s(&dir),
            to: ExtractTo::New("evil".into()),
            seven: None,
            log: s(&dir.join("job.log")),
        });
        assert!(r.is_err_and(|e| e.contains("outside the folder")));
        assert!(!dir.join("evil").exists() && !dir.join("evil.partial").exists());
        assert!(!dir.parent().is_some_and(|p| p.join("escaped.txt").exists()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_formats_need_7zip() {
        let dir = temp("7z");
        let s = |p: &Path| p.display().to_string();
        let _ = std::fs::write(dir.join("x.7z"), "not really");
        let (_, r) = run_job(&ArchiveJob::Extract {
            archive: s(&dir.join("x.7z")),
            dest_dir: s(&dir),
            to: ExtractTo::New("x".into()),
            seven: None,
            log: s(&dir.join("job.log")),
        });
        assert!(r.is_err_and(|e| e.contains("needs 7-Zip")));
        assert!(!dir.join("x.partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
