//! Running openers (CLAUDE.md section 13): finding programs (PATH, `App Paths`),
//! starting detached ones with their output in a log, the clipboard, UNC paths for
//! mapped drives, and the "last opener per extension" store.
//!
//! Everything here blocks: only the worker calls it.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Finds `program` like the Run box does: a path as given; else PATH (with PATHEXT);
/// else `App Paths`. None when it's nowhere.
pub fn resolve(program: &str) -> Option<String> {
    let p = program.trim();
    if p.is_empty() {
        return None;
    }
    if p.contains(['\\', '/']) {
        let expanded = crate::pins::expand_home(p).unwrap_or_else(|| p.to_string());
        return Path::new(&expanded).is_file().then_some(expanded);
    }
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.to_ascii_lowercase())
        .collect();
    let has_ext = Path::new(p).extension().is_some();
    let names: Vec<String> = if has_ext {
        vec![p.to_string()]
    } else {
        exts.iter().map(|e| format!("{p}{e}")).collect()
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for n in &names {
                let c = dir.join(n);
                if c.is_file() {
                    return Some(c.display().to_string());
                }
            }
        }
    }
    let exe = if has_ext {
        p.to_string()
    } else {
        format!("{p}.exe")
    };
    crate::platform::app_path(&exe)
        .filter(|a| Path::new(a).is_file())
        .or_else(|| program_files(p, &exe))
}

/// `%ProgramFiles%\<name>\<name>.exe` (and the x86 one): where Notepad++ and friends
/// install, for when they didn't register themselves.
fn program_files(name: &str, exe: &str) -> Option<String> {
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|base| Path::new(&base).join(stem).join(exe))
        .find(|c| c.is_file())
        .map(|c| c.display().to_string())
}

/// Every configured opener's program, resolved (None: not found, greyed in the menu).
pub fn resolve_all(cfg: &super::rules::OpenConfig) -> BTreeMap<String, Option<String>> {
    cfg.openers
        .iter()
        .map(|(name, o)| (name.clone(), o.run.first().and_then(|p| resolve(p))))
        .collect()
}

/// `openers.log` past this size becomes `openers.old.log` (replacing the previous one),
/// so the two never take more than about twice this.
const OPENER_LOG_MAX: u64 = 1024 * 1024;

/// `%LOCALAPPDATA%\sentier\logs\openers.log`: what detached programs print.
fn opener_log() -> Option<std::fs::File> {
    let dir = if cfg!(test) {
        std::env::temp_dir().join("sentier-test-logs")
    } else {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("logs")
    };
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("openers.log");
    rotate(&path, &dir.join("openers.old.log"), OPENER_LOG_MAX);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// `path` over `max` bytes: moved to `old` (replacing it), so writing starts afresh.
fn rotate(path: &std::path::Path, old: &std::path::Path, max: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max) {
        let _ = std::fs::remove_file(old);
        let _ = std::fs::rename(path, old);
    }
}

/// Starts `program args` in `cwd` and doesn't wait. No console window; its output goes
/// to the openers log, never to the TUI.
pub fn spawn_detached(program: &str, args: &[String], cwd: Option<&str>) -> io::Result<()> {
    // Tests exercise the paths that start programs; they must never open real windows.
    if cfg!(test) {
        return Ok(());
    }
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    match opener_log() {
        Some(log) => {
            let err = log.try_clone().map_or_else(|_| Stdio::null(), Stdio::from);
            cmd.stdout(Stdio::from(log)).stderr(err);
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    if let Some(dir) = cwd.filter(|d| Path::new(d).is_dir()) {
        cmd.current_dir(dir);
    }
    tracing::info!(program, ?args, ?cwd, "detached opener");
    cmd.spawn().map(|_| ())
}

pub fn copy_text(text: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.to_string()).map_err(|e| e.to_string())
}

/// `P:\x` → `\\srv01\aveva\x` when P: is a mapped drive; anything else unchanged. The
/// form to paste for someone without the same drive mappings.
pub fn unc_of(path: &str) -> String {
    let b = path.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let letter = (b[0] as char).to_ascii_uppercase();
        if let Some(target) = crate::platform::mapped_target(letter) {
            return format!("{}{}", target.trim_end_matches('\\'), &path[2..]);
        }
    }
    path.to_string()
}

// ------------------------------------------------------------------ last opener

/// Last opener per extension (`pdf` → `revu`).
pub type LastUsed = BTreeMap<String, String>;

fn last_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!(
            "sentier-test-open-last-{}.json",
            std::process::id()
        )));
    }
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("open_last.json"),
    )
}

pub fn load_last() -> LastUsed {
    last_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_last(last: &LastUsed) {
    let Some(path) = last_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_string_pretty(last) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// `%APPDATA%\sentier\open.toml` (a temp path in tests).
pub fn config_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir().join(format!("sentier-test-open-{}.toml", std::process::id())),
        );
    }
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join("sentier")
            .join("open.toml"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opener_log_rotates_past_its_limit() {
        let dir = std::env::temp_dir().join(format!("sentier-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        let (log, old) = (dir.join("openers.log"), dir.join("openers.old.log"));
        std::fs::write(&log, "small").ok();
        rotate(&log, &old, 10);
        assert!(log.exists() && !old.exists(), "under the limit: left alone");
        std::fs::write(&old, "older still").ok();
        std::fs::write(&log, "more than ten bytes").ok();
        rotate(&log, &old, 10);
        assert!(!log.exists());
        assert_eq!(
            std::fs::read_to_string(&old).ok().as_deref(),
            Some("more than ten bytes"),
            "the previous old log is replaced"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_finds_system_programs() {
        // cmd and explorer are on every Windows.
        assert!(resolve("cmd").is_some_and(|p| p.to_lowercase().ends_with("cmd.exe")));
        assert!(resolve("explorer.exe").is_some());
        assert_eq!(resolve("surely-not-a-program-xyz"), None);
        assert_eq!(resolve(r"C:\nope\nothing.exe"), None);
        assert_eq!(resolve("  "), None);
    }

    #[test]
    fn app_paths_registry() {
        assert_eq!(
            crate::platform::app_path("surely-not-registered-xyz.exe"),
            None
        );
        // Edge registers itself under App Paths on Windows 11; where it does, the path
        // must be a real file.
        if let Some(p) = crate::platform::app_path("msedge.exe") {
            assert!(Path::new(&p).is_file(), "{p}");
            assert!(resolve("msedge").is_some());
        }
    }

    #[test]
    fn unc_leaves_unmapped_paths() {
        assert_eq!(unc_of(r"\\srv01\p\x"), r"\\srv01\p\x");
        assert_eq!(unc_of(r"C:\Windows"), r"C:\Windows");
    }

    #[test]
    fn last_used_round_trips() {
        let mut last = LastUsed::new();
        last.insert("pdf".into(), "revu".into());
        save_last(&last);
        assert_eq!(load_last(), last);
        if let Some(p) = last_path() {
            let _ = std::fs::remove_file(p);
        }
    }
}
