//! Backups of `pins.toml` (the user's whole set-up: pins, tags, keys, presets, views).
//!
//! - Before sentier writes `pins.toml` (the app or the CLI), the file as it was is copied to
//!   `%APPDATA%\sentier\backups\pins-YYYYMMDD-HHMMSS.toml`, unless the newest copy is less
//!   than [`EVERY`] old or the same text. So a backup marks "before this editing session",
//!   and a burst of edits doesn't push the good versions out.
//! - The newest [`KEEP`] are kept.
//! - `:pins restore` lists them with what each would bring back; restoring first backs up
//!   the current file (always), then puts the chosen one in place.
//!
//! Blocking file I/O: only the worker and the CLI call this.

use chrono::NaiveDateTime;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Backups kept.
pub const KEEP: usize = 5;
/// At most one backup per this long, so one editing session is one backup.
pub const EVERY: Duration = Duration::from_secs(10 * 60);
const PREFIX: &str = "pins-";
const STAMP: &str = "%Y%m%d-%H%M%S";

/// `<folder of pins.toml>\backups`.
pub fn dir_for(pins_path: &Path) -> Option<PathBuf> {
    Some(pins_path.parent()?.join("backups"))
}

/// One backup file and when it was taken (from its name: copying keeps the source's
/// modified time, so the file's own time says nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub path: PathBuf,
    pub taken: NaiveDateTime,
}

fn taken_from_name(name: &str) -> Option<NaiveDateTime> {
    let stamp = name.strip_prefix(PREFIX)?.strip_suffix(".toml")?;
    // `pins-20261109-201500-1.toml` when two land in the same second.
    let stamp = stamp.get(..15)?;
    NaiveDateTime::parse_from_str(stamp, STAMP).ok()
}

/// The backups in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<Backup> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Backup> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            Some(Backup {
                taken: taken_from_name(&name)?,
                path: e.path(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.taken.cmp(&a.taken).then_with(|| b.path.cmp(&a.path)));
    out
}

/// Copies `pins_path` into its backups folder when it's due (see the module docs), or
/// always with `force` (before a restore). Returns the new backup, if one was made.
/// A missing `pins.toml` (nothing to lose yet) makes none.
pub fn snapshot(
    pins_path: &Path,
    now: NaiveDateTime,
    force: bool,
) -> Result<Option<PathBuf>, String> {
    let text = match std::fs::read_to_string(pins_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("pins.toml: {e}")),
    };
    let dir = dir_for(pins_path).ok_or("pins.toml has no folder")?;
    let existing = list(&dir);
    if let Some(newest) = existing.first() {
        let same = std::fs::read_to_string(&newest.path).is_ok_and(|t| t == text);
        let recent = now
            .signed_duration_since(newest.taken)
            .to_std()
            .is_ok_and(|age| age < EVERY);
        if same || (recent && !force) {
            return Ok(None);
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("backups folder: {e}"))?;
    let stamp = now.format(STAMP).to_string();
    let mut path = dir.join(format!("{PREFIX}{stamp}.toml"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{PREFIX}{stamp}-{n}.toml"));
        n += 1;
    }
    crate::fs::write_atomic(&path, &text).map_err(|e| format!("backup: {e}"))?;
    prune(&dir);
    Ok(Some(path))
}

/// Keeps the newest [`KEEP`].
fn prune(dir: &Path) {
    for old in list(dir).into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(old.path);
    }
}

/// What restoring a backup would change, by pin (kind and name, any case).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub pins: usize,
    pub now: usize,
    /// In the backup, not now: restoring brings them back.
    pub back: Vec<String>,
    /// Now, not in the backup: restoring removes them.
    pub gone: Vec<String>,
    /// In both, but different (path, tags, key…).
    pub changed: Vec<String>,
}

fn key(p: &crate::pins::Pin) -> String {
    format!("{}|{}", p.kind.label(), p.name.to_lowercase())
}

pub fn diff(backup: &crate::pins::PinStore, current: &crate::pins::PinStore) -> Diff {
    use std::collections::HashMap;
    let now: HashMap<String, &crate::pins::Pin> =
        current.pins.iter().map(|p| (key(p), p)).collect();
    let then: HashMap<String, &crate::pins::Pin> =
        backup.pins.iter().map(|p| (key(p), p)).collect();
    let mut d = Diff {
        pins: backup.pins.len(),
        now: current.pins.len(),
        ..Diff::default()
    };
    for p in &backup.pins {
        match now.get(&key(p)) {
            None => d.back.push(p.name.clone()),
            Some(q) if *q != p => d.changed.push(p.name.clone()),
            Some(_) => {}
        }
    }
    for p in &current.pins {
        if !then.contains_key(&key(p)) {
            d.gone.push(p.name.clone());
        }
    }
    d
}

impl Diff {
    /// `312 pins · 3 back · 1 gone · 2 changed`, or `312 pins · same as now`.
    pub fn summary(&self) -> String {
        let mut parts = vec![crate::pins::pins_word(self.pins)];
        if self.back.is_empty() && self.gone.is_empty() && self.changed.is_empty() {
            parts.push("same as now".into());
        }
        for (n, what) in [
            (self.back.len(), "back"),
            (self.gone.len(), "gone"),
            (self.changed.len(), "changed"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        parts.join(" · ")
    }
}

/// A backup, read and compared with the current pins, for the restore picker.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub backup: Backup,
    /// Err: the file doesn't load (shown, not restorable).
    pub diff: Result<Diff, String>,
}

/// Every backup next to `pins_path`, newest first, each compared with the current file.
pub fn listed(pins_path: &Path) -> Vec<Listed> {
    let current = crate::pins::load(pins_path)
        .map(|(s, _)| s)
        .unwrap_or_default();
    let Some(dir) = dir_for(pins_path) else {
        return Vec::new();
    };
    list(&dir)
        .into_iter()
        .map(|backup| {
            let diff = crate::pins::load(&backup.path).map(|(s, _)| diff(&s, &current));
            Listed { backup, diff }
        })
        .collect()
}

/// Puts `backup` in place of `pins_path`, after backing up the current file (always).
/// Returns the store it now holds.
pub fn restore(
    pins_path: &Path,
    backup: &Path,
    now: NaiveDateTime,
) -> Result<crate::pins::PinStore, String> {
    let text = std::fs::read_to_string(backup).map_err(|e| format!("backup: {e}"))?;
    // Refuse a file that doesn't load rather than replace good pins with it.
    let (store, _) = crate::pins::parse(&text)?;
    snapshot(pins_path, now, true)?;
    crate::fs::write_atomic(pins_path, &text).map_err(|e| format!("pins.toml: {e}"))?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap_or_default()
    }

    fn folder(name: &str, path: &str) -> String {
        format!("[[pin]]\nkind = \"folder\"\nname = \"{name}\"\npath = '{path}'\n\n")
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sentier-pinbak-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d.join("pins.toml")
    }

    #[test]
    fn one_backup_per_session_newest_five_kept() {
        let pins = temp("keep");
        // Nothing to lose yet: no backup.
        assert_eq!(snapshot(&pins, at("2026-11-09 08:00:00"), false), Ok(None));
        std::fs::write(&pins, folder("A", r"C:\a")).ok();
        let first = snapshot(&pins, at("2026-11-09 08:00:00"), false);
        assert!(matches!(first, Ok(Some(_))), "{first:?}");
        // 5 minutes later, changed: same session, no new backup.
        std::fs::write(&pins, folder("B", r"C:\b")).ok();
        assert_eq!(snapshot(&pins, at("2026-11-09 08:05:00"), false), Ok(None));
        // Unchanged text never makes a duplicate, even much later.
        std::fs::write(&pins, folder("A", r"C:\a")).ok();
        assert_eq!(snapshot(&pins, at("2026-11-09 12:00:00"), false), Ok(None));
        // A restore always backs up first.
        std::fs::write(&pins, folder("C", r"C:\c")).ok();
        assert!(matches!(
            snapshot(&pins, at("2026-11-09 12:01:00"), true),
            Ok(Some(_))
        ));
        // Hours apart: a new one each time, only the newest five kept.
        for h in 13..20 {
            std::fs::write(&pins, folder(&format!("P{h}"), r"C:\p")).ok();
            let now = at(&format!("2026-11-09 {h}:00:00"));
            assert!(matches!(snapshot(&pins, now, false), Ok(Some(_))));
        }
        let dir = dir_for(&pins).unwrap_or_default();
        let kept = list(&dir);
        assert_eq!(kept.len(), KEEP);
        assert_eq!(kept[0].taken, at("2026-11-09 19:00:00"), "newest first");
        assert_eq!(kept[4].taken, at("2026-11-09 15:00:00"));
        let _ = std::fs::remove_dir_all(pins.parent().unwrap_or(&pins));
    }

    #[test]
    fn restore_backs_up_the_current_pins_and_refuses_a_broken_file() {
        let pins = temp("restore");
        let dir = dir_for(&pins).unwrap_or_default();
        std::fs::create_dir_all(&dir).ok();
        let good = dir.join("pins-20261101-090000.toml");
        std::fs::write(
            &good,
            folder("Old", r"\\srv01\old") + &folder("Kept", r"C:\k"),
        )
        .ok();
        std::fs::write(&pins, folder("Kept", r"C:\k") + &folder("New", r"C:\n")).ok();

        let shown = listed(&pins);
        assert_eq!(shown.len(), 1);
        let d = shown[0].diff.clone().unwrap_or_default();
        assert_eq!(
            (d.back.clone(), d.gone.clone()),
            (vec!["Old".to_string()], vec!["New".to_string()])
        );
        assert_eq!(d.summary(), "2 pins · 1 back · 1 gone");

        let store = restore(&pins, &good, at("2026-11-09 20:00:00"));
        assert_eq!(store.map(|s| s.pins.len()), Ok(2));
        let text = std::fs::read_to_string(&pins).unwrap_or_default();
        assert!(
            text.contains(r"\\srv01\old"),
            "the backup is in place, backslashes intact"
        );
        let kept = list(&dir);
        assert_eq!(kept.len(), 2, "the pins it replaced were backed up first");
        let before = std::fs::read_to_string(&kept[0].path).unwrap_or_default();
        assert!(before.contains("New"));

        let broken = dir.join("pins-20261102-090000.toml");
        std::fs::write(&broken, "[[pin]\nkind = ").ok();
        assert!(restore(&pins, &broken, at("2026-11-09 21:00:00")).is_err());
        assert!(
            std::fs::read_to_string(&pins)
                .unwrap_or_default()
                .contains("Old"),
            "untouched"
        );
        let _ = std::fs::remove_dir_all(pins.parent().unwrap_or(&pins));
    }

    #[test]
    fn names_and_changes() {
        assert_eq!(
            taken_from_name("pins-20261109-201500.toml"),
            Some(at("2026-11-09 20:15:00"))
        );
        assert_eq!(
            taken_from_name("pins-20261109-201500-2.toml"),
            Some(at("2026-11-09 20:15:00"))
        );
        assert_eq!(taken_from_name("notes.toml"), None);
        let (a, _) = crate::pins::parse(&folder("X", r"C:\x")).unwrap_or_default();
        let (b, _) = crate::pins::parse(&folder("x", r"C:\y")).unwrap_or_default();
        let d = diff(&a, &b);
        assert_eq!(d.changed, ["X"], "same name (any case), other path");
        assert_eq!(d.summary(), "1 pin · 1 changed");
        assert_eq!(diff(&a, &a).summary(), "1 pin · same as now");
    }
}
