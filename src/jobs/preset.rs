//! Saved sync presets (CLAUDE.md section 9): a `kind = "sync"` pin read as a preset, the
//! dry-run preview built from robocopy's `/L` output, and the last-run state kept in
//! `%LOCALAPPDATA%\sentier\presets.json`.
//!
//! Safety rules live here so every caller (the TUI, `sentier run`) gets them:
//! - `source` and `dest` may not be the same folder or contain each other;
//! - the dry run always uses `/MIR /L`, so it lists the local files only in `dest` in
//!   both modes: mirror deletes them, update keeps them. Switching modes in the preview
//!   needs no second robocopy pass.
//!
//! Pure except [`load_runs`] and [`save_runs`], which block; only the worker calls them.

use super::robocopy::{FileClass, Step, Tracker};
use super::{Phase, Report, human_bytes};
use crate::fs::path::{FsPath, Loc};
use crate::pins::{Pin, PinKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    /// `/MIR`: dest becomes an exact copy; local files not on the server are deleted.
    Mirror,
    /// `/E`: new and changed files are copied; nothing in dest is deleted.
    Update,
}

impl SyncMode {
    pub fn word(self) -> &'static str {
        match self {
            SyncMode::Mirror => "mirror",
            SyncMode::Update => "update",
        }
    }

    pub fn flip(self) -> SyncMode {
        match self {
            SyncMode::Mirror => SyncMode::Update,
            SyncMode::Update => SyncMode::Mirror,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncPreset {
    pub name: String,
    pub source: FsPath,
    pub dest: FsPath,
    pub mode: SyncMode,
    /// robocopy `/XD`.
    pub exclude_dirs: Vec<String>,
    /// robocopy `/XF`.
    pub exclude_files: Vec<String>,
    /// Only these, if any (names in the source, folders ending in ``): what was
    /// selected when the preset was made. Each folder is its own step; the files one more.
    pub include: Vec<String>,
}

fn fs_path(field: &str, text: &str) -> Result<FsPath, String> {
    let text = crate::pins::expand_home(text).ok_or_else(|| format!("{field}: no home folder"))?;
    match Loc::parse(&text) {
        Ok(Loc::Fs(p)) => Ok(p),
        Ok(_) => Err(format!("{field} must be a folder path: {text}")),
        Err(e) => Err(format!("{field}: {e}")),
    }
}

fn strings(pin: &Pin, key: &str) -> Result<Vec<String>, String> {
    match pin.extra.get(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::String(s)) => Ok(vec![s.clone()]),
        Some(toml::Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{key} must be a list of strings"))
            })
            .collect(),
        Some(_) => Err(format!("{key} must be a list of strings")),
    }
}

/// `include`: names directly in the source (a folder ends in `\`), no paths, no `..`.
fn includes(pin: &Pin) -> Result<Vec<String>, String> {
    let list = strings(pin, "include")?;
    for i in &list {
        let name = i.strip_suffix('\\').unwrap_or(i);
        if name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/']) {
            return Err(format!(
                "include {i:?}: names in the source folder (a folder ends in \\)"
            ));
        }
    }
    Ok(list)
}

/// Refuses a preset whose folders are the same or nest: a sync must never be able to
/// overwrite its own source.
pub fn check_paths(source: &FsPath, dest: &FsPath) -> Result<(), String> {
    if source.starts_with(dest) && dest.starts_with(source) {
        return Err("source and dest are the same folder".into());
    }
    if dest.starts_with(source) {
        return Err("dest is inside source".into());
    }
    if source.starts_with(dest) {
        return Err("source is inside dest".into());
    }
    Ok(())
}

impl SyncPreset {
    pub fn from_pin(pin: &Pin) -> Result<SyncPreset, String> {
        if pin.kind != PinKind::Sync {
            return Err(format!("{} is not a sync preset", pin.name));
        }
        let get = |k: &str| pin.extra.get(k).and_then(|v| v.as_str());
        let source = fs_path("source", get("source").ok_or("sync preset needs a source")?)?;
        let dest = fs_path("dest", get("dest").ok_or("sync preset needs a dest")?)?;
        check_paths(&source, &dest)?;
        let mode = match get("mode")
            .map(|m| m.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("update") => SyncMode::Update,
            Some("mirror") => SyncMode::Mirror,
            Some(other) => return Err(format!("mode {other:?}: mirror or update")),
        };
        Ok(SyncPreset {
            name: pin.name.clone(),
            source,
            dest,
            mode,
            exclude_dirs: strings(pin, "exclude_dirs")?,
            exclude_files: strings(pin, "exclude_files")?,
            include: includes(pin)?,
        })
    }

    /// What the last-run state is kept under: the folders, not the name, so a rename
    /// keeps the history.
    pub fn key(&self) -> String {
        format!("sync|{}|{}", self.source.display(), self.dest.display()).to_lowercase()
    }

    /// The whole source to the whole dest (also how preview paths are shown).
    pub fn step(&self) -> Step {
        Step {
            src: self.source.display(),
            dst: self.dest.display(),
            files: Vec::new(),
        }
    }

    /// The robocopy steps: the whole folder, or with `include`, one per included folder
    /// (mirrored within itself in mirror mode) and one for the included files (copied,
    /// never a purge: robocopy only mirrors whole folders).
    pub fn steps(&self) -> Vec<Step> {
        if self.include.is_empty() {
            return vec![self.step()];
        }
        let src = self.source.display();
        let dst = self.dest.display();
        let join = |base: &str, name: &str| format!("{}\\{name}", base.trim_end_matches('\\'));
        let mut steps: Vec<Step> = self
            .include
            .iter()
            .filter_map(|i| i.strip_suffix('\\'))
            .map(|dir| Step {
                src: join(&src, dir),
                dst: join(&dst, dir),
                files: Vec::new(),
            })
            .collect();
        let files: Vec<String> = self
            .include
            .iter()
            .filter(|i| !i.ends_with('\\'))
            .cloned()
            .collect();
        if !files.is_empty() {
            steps.push(Step { src, dst, files });
        }
        steps
    }

    /// `sync · mirror`, for the job list.
    pub fn label(mode: SyncMode) -> String {
        format!("sync · {}", mode.word())
    }
}

/// A job started from a preset: where its last-run state goes, and its label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetRef {
    pub key: String,
    /// `sync · mirror`.
    pub label: String,
}

// ------------------------------------------------------------------ the dry run

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Not in dest yet.
    New,
    /// In dest, but differs: the local copy is overwritten.
    Replace,
    /// Only in dest: deleted by mirror, kept by update.
    Extra,
}

impl Change {
    pub fn sign(self) -> char {
        match self {
            Change::New => '+',
            Change::Replace => '~',
            Change::Extra => '-',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub change: Change,
    pub size: u64,
    /// Relative to source (or dest, for extras): `hitiso\old\B-1001_rev3.dgn`.
    pub rel: String,
}

/// What a sync would do, from a `/MIR /L` pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preview {
    /// New, then replaced, then extras; each in robocopy's order.
    pub rows: Vec<Row>,
    /// Folders only in dest (mirror deletes them with what's inside).
    pub extra_dirs: Vec<String>,
    /// Files robocopy left alone (same size and time).
    pub unchanged: u64,
    pub errors: Vec<String>,
    pub exit: i32,
}

/// `C:\a\b\c.txt` relative to `C:\a` → `b\c.txt`; unchanged if it isn't below it.
fn relative(path: &str, base: &str) -> String {
    let base = base.trim_end_matches('\\');
    match path.get(..base.len()) {
        Some(head) if head.eq_ignore_ascii_case(base) => {
            path[base.len()..].trim_start_matches('\\').to_string()
        }
        _ => path.to_string(),
    }
}

impl Preview {
    pub fn from_tracker(t: &Tracker, exit: i32, step: &Step) -> Preview {
        let mut rows: Vec<Row> = Vec::new();
        let listed = |want: Change| {
            t.listed.iter().filter_map(move |(class, size, path)| {
                let change = match class {
                    FileClass::Same => return None,
                    FileClass::New | FileClass::Other(_) => Change::New,
                    _ => Change::Replace,
                };
                (change == want).then(|| Row {
                    change,
                    size: *size,
                    rel: relative(path, &step.src),
                })
            })
        };
        rows.extend(listed(Change::New));
        rows.extend(listed(Change::Replace));
        rows.extend(t.extras.iter().map(|(size, path)| Row {
            change: Change::Extra,
            size: *size,
            rel: relative(path, &step.dst),
        }));
        let mut errors: Vec<String> = t
            .errors
            .iter()
            .map(|e| match &e.path {
                Some(p) => format!("ERROR {} {} {p}: {}", e.code, e.action, e.message),
                None => format!("ERROR {} {}: {}", e.code, e.action, e.message),
            })
            .collect();
        errors.dedup();
        Preview {
            rows,
            extra_dirs: t
                .extra_dirs
                .iter()
                .map(|d| relative(d, &step.dst))
                .collect(),
            unchanged: t.summary.map_or(0, |s| s.files.skipped),
            errors,
            exit,
        }
    }

    /// (files, bytes) of one kind of change.
    pub fn count(&self, change: Change) -> (u64, u64) {
        self.rows
            .iter()
            .filter(|r| r.change == change)
            .fold((0, 0), |(n, b), r| (n + 1, b + r.size))
    }

    /// (files, bytes) a run copies: new + replaced.
    pub fn to_copy(&self) -> (u64, u64) {
        let (a, b) = self.count(Change::New);
        let (c, d) = self.count(Change::Replace);
        (a + c, b + d)
    }

    /// Why it can't run: robocopy failed (8 and up) or reported errors.
    pub fn problem(&self) -> Option<String> {
        if let Some(e) = self.errors.first() {
            return Some(e.clone());
        }
        (self.exit >= 8 || self.exit < 0).then(|| {
            format!(
                "the dry run failed: robocopy exit {} = {}",
                self.exit,
                super::robocopy::exit_meaning(self.exit).parts
            )
        })
    }

    /// Nothing to copy, and (in mirror) nothing to delete.
    pub fn is_noop(&self, mode: SyncMode) -> bool {
        self.rows
            .iter()
            .all(|r| r.change == Change::Extra && mode == SyncMode::Update)
            && (mode == SyncMode::Update || self.extra_dirs.is_empty())
    }

    /// One line: `412 to copy (3.80 GB) · 17 replace local · 6 DELETED (mirror)`.
    pub fn summary(&self, mode: SyncMode) -> String {
        let (n, b) = self.to_copy();
        let (r, _) = self.count(Change::Replace);
        let (x, _) = self.count(Change::Extra);
        let mut s = format!("{n} to copy ({})", human_bytes(b));
        if r > 0 {
            s.push_str(&format!(" · {r} replace local"));
        }
        if x > 0 {
            match mode {
                SyncMode::Mirror => s.push_str(&format!(" · {x} DELETED (mirror)")),
                SyncMode::Update => s.push_str(&format!(" · {x} local only, kept")),
            }
        }
        s
    }
}

// ------------------------------------------------------------------ last run

/// How a preset's last run went, shown on its pin: `last: today 08:12 ✓ 3.80 GB in 2m41s`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    /// Unix seconds, when it started.
    pub at: i64,
    pub phase: Phase,
    pub files: u64,
    pub bytes: u64,
    pub seconds: u64,
    pub failed: u64,
    /// Failed because another process had them open (ERROR 32 / 33).
    pub locked: u64,
}

impl LastRun {
    pub fn from_report(r: &Report) -> LastRun {
        LastRun {
            at: r
                .started
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
            phase: r.phase,
            // A backup is its zip: entries and size.
            files: r.backup.as_ref().map_or(r.copied, |b| b.entries),
            bytes: r.backup.as_ref().map_or(r.bytes, |b| b.bytes),
            seconds: r.seconds,
            failed: r.failed.len() as u64,
            locked: r
                .failed
                .iter()
                .filter(|f| matches!(f.code, 32 | 33))
                .count() as u64,
        }
    }

    /// `today 08:12 ✓ 3.80 GB in 2m41s`, `yesterday 17:03 ✗ failed (2 locked)`.
    pub fn line(&self, now: chrono::DateTime<chrono::Local>) -> String {
        let when = chrono::DateTime::from_timestamp(self.at, 0)
            .map(|t| t.with_timezone(&chrono::Local))
            .map_or_else(
                || "?".to_string(),
                |t| {
                    let days = (now.date_naive() - t.date_naive()).num_days();
                    let day = match days {
                        0 => "today".to_string(),
                        1 => "yesterday".to_string(),
                        _ => t.format("%Y-%m-%d").to_string(),
                    };
                    format!("{day} {}", t.format("%H:%M"))
                },
            );
        let what = match self.phase {
            Phase::Done if self.files == 0 => "✓ nothing to copy".to_string(),
            Phase::Done => format!(
                "✓ {} in {}",
                human_bytes(self.bytes),
                short_duration(self.seconds)
            ),
            Phase::Cancelled => "cancelled".to_string(),
            _ if self.locked > 0 => format!("✗ failed ({} locked)", self.locked),
            _ if self.failed > 0 => format!("✗ failed ({} files)", self.failed),
            _ => "✗ failed".to_string(),
        };
        format!("{when} {what}")
    }
}

/// `41s`, `2m41s`, `1h02m`.
pub fn short_duration(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// Last runs by [`SyncPreset::key`].
pub type Runs = BTreeMap<String, LastRun>;

/// `%LOCALAPPDATA%\sentier\presets.json`.
pub fn runs_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join("sentier-test-presets.json"));
    }
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("sentier").join("presets.json"))
}

/// A missing file is no runs yet; a broken one is logged and treated the same.
pub fn load_runs(path: &Path) -> Runs {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Runs::new();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!(file = %path.display(), error = %e, "bad presets.json");
        Runs::new()
    })
}

/// Atomic: written next to it, then renamed over it.
pub fn save_runs(path: &Path, runs: &Runs) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(runs).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::robocopy::{Event, Row as RcRow, Summary};
    use super::*;

    fn pin(toml_text: &str) -> Pin {
        match toml::from_str::<Pin>(toml_text) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        }
    }

    fn p(s: &str) -> FsPath {
        let Ok(Loc::Fs(f)) = Loc::parse(s) else {
            panic!("{s}")
        };
        f
    }

    #[test]
    fn preset_from_pin() {
        let pr = SyncPreset::from_pin(&pin(r#"
            kind = "sync"
            name = "HIT → local"
            source = '\\srv01\project\HIT'
            dest = 'D:\aveva\local\HIT'
            mode = "Mirror"
            exclude_dirs = ["tmp", "old"]
            exclude_files = "*.bak"
        "#));
        let Ok(pr) = pr else { panic!("{pr:?}") };
        assert_eq!(pr.mode, SyncMode::Mirror);
        assert_eq!(pr.exclude_dirs, ["tmp", "old"]);
        assert_eq!(pr.exclude_files, ["*.bak"]);
        assert_eq!(pr.key(), r"sync|\\srv01\project\hit|d:\aveva\local\hit");
        assert_eq!(pr.step().src, r"\\srv01\project\HIT");
        assert_eq!(SyncPreset::label(pr.mode), "sync · mirror");

        // No mode: update, the safe one.
        let pr = SyncPreset::from_pin(&pin(
            "kind = 'sync'\nname = 'x'\nsource = 'C:\\a'\ndest = 'D:\\a'",
        ));
        assert_eq!(pr.map(|p| p.mode), Ok(SyncMode::Update));
    }

    #[test]
    fn refusals() {
        let table = [
            (r"\\srv01\p\HIT", r"\\SRV01\P\hit", "same folder"),
            (
                r"\\srv01\p\HIT",
                r"\\srv01\p\HIT\local",
                "dest is inside source",
            ),
            (r"D:\aveva", r"D:\", "source is inside dest"),
        ];
        for (src, dst, why) in table {
            let r = check_paths(&p(src), &p(dst));
            assert!(
                r.as_ref().is_err_and(|e| e.contains(why)),
                "{src} {dst}: {r:?}"
            );
        }
        assert_eq!(check_paths(&p(r"\\srv01\p\HIT"), &p(r"D:\HIT")), Ok(()));
        assert_eq!(
            check_paths(&p(r"C:\ab"), &p(r"C:\a")),
            Ok(()),
            "not a prefix by name"
        );

        let bad = [
            ("kind = 'folder'\nname = 'x'\npath = 'C:\\a'", "not a sync"),
            (
                "kind = 'sync'\nname = 'x'\ndest = 'D:\\a'",
                "needs a source",
            ),
            (
                "kind = 'sync'\nname = 'x'\nsource = 'C:\\a'\ndest = 'D:\\a'\nmode = 'push'",
                "mirror or update",
            ),
            (
                "kind = 'sync'\nname = 'x'\nsource = 'C:\\a'\ndest = 'D:\\a'\nexclude_dirs = [1]",
                "list of strings",
            ),
            (
                "kind = 'sync'\nname = 'x'\nsource = 'C:\\a'\ndest = 'C:\\a\\b'",
                "inside",
            ),
        ];
        for (text, why) in bad {
            let r = SyncPreset::from_pin(&pin(text));
            assert!(r.as_ref().is_err_and(|e| e.contains(why)), "{text}: {r:?}");
        }
    }

    fn fixture_preview(name: &str) -> Preview {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("robocopy");
        let out = std::fs::read(dir.join(format!("{name}.out"))).unwrap_or_default();
        let exit: i32 = std::fs::read_to_string(dir.join(format!("{name}.exit")))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(-1);
        let mut parser = super::super::robocopy::Parser::new();
        let mut t = Tracker::new(true);
        let text = String::from_utf8_lossy(&out);
        for e in parser.feed(&text).into_iter().chain(parser.finish()) {
            t.apply(e);
        }
        let step = Step {
            src: format!(r"C:\capture\{name}\src"),
            dst: format!(r"C:\capture\{name}\dst"),
            files: vec![],
        };
        Preview::from_tracker(&t, exit, &step)
    }

    #[test]
    fn preview_from_the_list_fixture() {
        let pv = fixture_preview("list");
        assert_eq!(pv.problem(), None);
        assert_eq!(pv.count(Change::New).0, 3);
        assert_eq!(
            pv.count(Change::Replace),
            (1, 6),
            "Older: the local copy is replaced"
        );
        assert_eq!(pv.count(Change::Extra), (1, 5));
        assert_eq!(pv.to_copy(), (4, 41_943_050));
        assert_eq!(pv.rows[0].rel, r"sub\b.txt");
        assert_eq!(pv.rows.last().map(|r| r.rel.as_str()), Some("extra.txt"));
        assert_eq!(
            pv.summary(SyncMode::Mirror),
            "4 to copy (40.00 MB) · 1 replace local · 1 DELETED (mirror)"
        );
        assert!(pv.summary(SyncMode::Update).ends_with("1 local only, kept"));
        assert!(!pv.is_noop(SyncMode::Update));
    }

    #[test]
    fn preview_extra_dirs_and_failures() {
        // A real /MIR run's output has the same lines a /L pass would.
        let pv = fixture_preview("mirror_extra");
        assert_eq!(pv.extra_dirs, [r"old\"]);
        assert_eq!(pv.count(Change::Extra).0, 2);

        let pv = fixture_preview("missing_source");
        assert!(pv.problem().is_some_and(|e| e.contains("ERROR")), "{pv:?}");

        // Only local extras: update has nothing to do, mirror does.
        let mut t = Tracker::new(true);
        t.apply(Event::File {
            class: FileClass::Extra,
            size: 3,
            path: r"D:\x\old.txt".into(),
        });
        t.apply(Event::Summary(Summary {
            files: RcRow {
                skipped: 7,
                ..RcRow::default()
            },
            ..Summary::default()
        }));
        let step = Step {
            src: r"\\s\p\x".into(),
            dst: r"D:\x".into(),
            files: vec![],
        };
        let pv = Preview::from_tracker(&t, 2, &step);
        assert!(pv.is_noop(SyncMode::Update));
        assert!(!pv.is_noop(SyncMode::Mirror));
        assert_eq!(pv.unchanged, 7);
        assert_eq!(pv.rows[0].rel, "old.txt");
    }

    #[test]
    fn last_run_lines() {
        use chrono::TimeZone;
        let Some(now) = chrono::Local
            .with_ymd_and_hms(2026, 11, 6, 9, 0, 0)
            .single()
        else {
            panic!()
        };
        let at = |d: u32, h: u32, m: u32| {
            chrono::Local
                .with_ymd_and_hms(2026, 11, d, h, m, 0)
                .single()
                .map_or(0, |t| t.timestamp())
        };
        let ok = LastRun {
            at: at(6, 8, 12),
            phase: Phase::Done,
            files: 412,
            bytes: 4_080_218_931,
            seconds: 161,
            failed: 0,
            locked: 0,
        };
        assert_eq!(ok.line(now), "today 08:12 ✓ 3.80 GB in 2m41s");
        let failed = LastRun {
            at: at(5, 17, 3),
            phase: Phase::Failed,
            failed: 2,
            locked: 2,
            ..ok.clone()
        };
        assert_eq!(failed.line(now), "yesterday 17:03 ✗ failed (2 locked)");
        let old = LastRun {
            at: at(2, 7, 0),
            files: 0,
            ..ok
        };
        assert_eq!(old.line(now), "2026-11-02 07:00 ✓ nothing to copy");
        assert_eq!(short_duration(41), "41s");
        assert_eq!(short_duration(3723), "1h02m");
    }

    #[test]
    fn runs_round_trip() {
        let path = std::env::temp_dir().join(format!("sentier-runs-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(load_runs(&path).is_empty());
        let mut runs = Runs::new();
        runs.insert(
            "sync|a|b".into(),
            LastRun {
                at: 1,
                phase: Phase::Done,
                files: 1,
                bytes: 2,
                seconds: 3,
                failed: 0,
                locked: 0,
            },
        );
        assert_eq!(save_runs(&path, &runs), Ok(()));
        assert_eq!(load_runs(&path), runs);
        std::fs::write(&path, "{").ok();
        assert!(
            load_runs(&path).is_empty(),
            "a broken file is no runs, not a crash"
        );
        let _ = std::fs::remove_file(&path);
    }
}
