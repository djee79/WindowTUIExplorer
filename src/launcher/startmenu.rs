//! The Start Menu's apps (ported from wtm's `scan_start_menu`): every `.lnk` under the
//! per-user, then the all-users Start Menu, 4 folders deep, uninstallers left out,
//! deduplicated by name (the per-user one wins). ShellExecute runs a `.lnk` as the Start
//! Menu does.
//!
//! Blocking (folder reads): worker only.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// (name, `.lnk` path), A→Z.
pub type Apps = Vec<(String, String)>;

fn collect(dir: &Path, depth: u32, out: &mut Apps) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect(&p, depth + 1, out);
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
        {
            let low = stem.to_lowercase();
            // Nobody wants the uninstaller in a launcher (wtm; French Windows too).
            if low.contains("uninstall") || low.contains("désinstall") {
                continue;
            }
            out.push((stem.to_string(), p.display().to_string()));
        }
    }
}

/// Scans `roots` in order (tests pass their own).
pub fn scan_roots(roots: &[PathBuf]) -> Apps {
    let mut raw = Vec::new();
    for root in roots {
        collect(root, 0, &mut raw);
    }
    let mut seen = HashSet::new();
    let mut out: Apps = raw
        .into_iter()
        .filter(|(name, _)| seen.insert(name.to_lowercase()))
        .collect();
    out.sort_by_key(|(name, _)| name.to_lowercase());
    out
}

pub fn scan() -> Apps {
    let programs = r"Microsoft\Windows\Start Menu\Programs";
    let roots: Vec<PathBuf> = ["APPDATA", "ProgramData"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|d| PathBuf::from(d).join(programs))
        .collect();
    scan_roots(&roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_dedupes_and_skips_uninstallers() {
        let dir = std::env::temp_dir().join(format!("sentier-startmenu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let user = dir.join("user");
        let all = dir.join("all");
        for (d, f) in [
            (user.join("Tools"), "Notepad++.lnk"),
            (user.clone(), "WezTerm.lnk"),
            (all.join("AVEVA"), "E3D Design.lnk"),
            (all.join("AVEVA"), "Uninstall E3D.lnk"),
            (all.clone(), "wezterm.lnk"),
            (all.clone(), "readme.txt"),
        ] {
            let _ = std::fs::create_dir_all(&d);
            let _ = std::fs::write(d.join(f), "");
        }
        let apps = scan_roots(&[user.clone(), all]);
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["E3D Design", "Notepad++", "WezTerm"]);
        assert!(
            apps[2].1.starts_with(&user.display().to_string()),
            "per-user wins"
        );
        // The real Start Menu has something on any Windows.
        assert!(!scan().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
