//! File operations (CLAUDE.md section 5): rename, new file / folder, delete to the Recycle
//! Bin, permanent delete, bulk rename. The checks and plans are pure; [`FileOp::run`]
//! does the work and blocks, so only the worker calls it.

use std::io;
use std::path::Path;

/// One change to the file system, with full paths (display form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// `dd` on a local disk.
    Trash(Vec<String>),
    /// `D`, or `dd` where there is no Recycle Bin (a share, a mapped drive).
    Delete(Vec<String>),
    Rename {
        from: String,
        to: String,
    },
    /// `A`, or `a` with a trailing `\`. Missing parents are made too.
    Mkdir(String),
    /// `a`. Missing parent folders (`sub\new.txt`) are made too.
    NewFile(String),
    /// `:rename`: (old, new) full paths, applied through temporary names so swaps work.
    BulkRename(Vec<(String, String)>),
}

/// How it went: what to say, and the name to put the cursor on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub message: String,
    pub select: Option<String>,
}

fn name_of(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn count(n: usize) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{n} items")
    }
}

impl FileOp {
    /// `rename a.txt → b.txt`, `delete 3 items permanently`.
    pub fn describe(&self) -> String {
        match self {
            FileOp::Trash(p) if p.len() == 1 => {
                format!("move {} to the Recycle Bin", name_of(&p[0]))
            }
            FileOp::Trash(p) => format!("move {} to the Recycle Bin", count(p.len())),
            FileOp::Delete(p) if p.len() == 1 => format!("delete {} permanently", name_of(&p[0])),
            FileOp::Delete(p) => format!("delete {} permanently", count(p.len())),
            FileOp::Rename { from, to } => format!("rename {} → {}", name_of(from), name_of(to)),
            FileOp::Mkdir(p) => format!("create folder {}", name_of(p)),
            FileOp::NewFile(p) => format!("create file {}", name_of(p)),
            FileOp::BulkRename(pairs) => format!("rename {}", count(pairs.len())),
        }
    }

    /// The paths it changes (for the audit log).
    pub fn paths(&self) -> Vec<String> {
        match self {
            FileOp::Trash(p) | FileOp::Delete(p) => p.clone(),
            FileOp::Rename { from, to } => vec![format!("{from} -> {to}")],
            FileOp::Mkdir(p) | FileOp::NewFile(p) => vec![p.clone()],
            FileOp::BulkRename(pairs) => pairs.iter().map(|(a, b)| format!("{a} -> {b}")).collect(),
        }
    }

    /// Does it. Blocking.
    pub fn run(&self) -> Result<Done, String> {
        match self {
            FileOp::Trash(paths) => {
                trash::delete_all(paths).map_err(|e| format!("Recycle Bin: {e}"))?;
                Ok(Done {
                    message: format!("{} moved to the Recycle Bin", count(paths.len())),
                    select: None,
                })
            }
            FileOp::Delete(paths) => {
                let mut failed = Vec::new();
                for p in paths {
                    if let Err(e) = remove(Path::new(p)) {
                        failed.push(format!("{}: {}", name_of(p), os_message(&e)));
                    }
                }
                if failed.is_empty() {
                    Ok(Done {
                        message: format!("{} deleted", count(paths.len())),
                        select: None,
                    })
                } else {
                    Err(format!(
                        "{} of {} not deleted: {}",
                        failed.len(),
                        paths.len(),
                        failed.join("; ")
                    ))
                }
            }
            FileOp::Rename { from, to } => {
                // A case-only rename (`a.txt` → `A.txt`) is the same file: allowed.
                if Path::new(to).exists() && !from.eq_ignore_ascii_case(to) {
                    return Err(format!("{} already exists", name_of(to)));
                }
                std::fs::rename(from, to).map_err(|e| os_message(&e))?;
                Ok(Done {
                    message: format!("renamed to {}", name_of(to)),
                    select: Some(name_of(to).to_string()),
                })
            }
            FileOp::Mkdir(p) => {
                if Path::new(p).exists() {
                    return Err(format!("{} already exists", name_of(p)));
                }
                std::fs::create_dir_all(p).map_err(|e| os_message(&e))?;
                Ok(Done {
                    message: format!("created folder {}", name_of(p)),
                    select: Some(first_new(p)),
                })
            }
            FileOp::NewFile(p) => {
                if let Some(parent) = Path::new(p).parent() {
                    std::fs::create_dir_all(parent).map_err(|e| os_message(&e))?;
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(p)
                    .map_err(|e| match e.kind() {
                        io::ErrorKind::AlreadyExists => format!("{} already exists", name_of(p)),
                        _ => os_message(&e),
                    })?;
                Ok(Done {
                    message: format!("created {}", name_of(p)),
                    select: Some(first_new(p)),
                })
            }
            FileOp::BulkRename(pairs) => bulk_rename(pairs),
        }
    }
}

/// The cursor goes on what appeared in the current folder: for `sub\x.txt`, `sub`.
fn first_new(p: &str) -> String {
    name_of(p).to_string()
}

/// A folder (with everything in it) or a file. Read-only files are made writable first,
/// as Explorer does.
pub(crate) fn remove(p: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        match std::fs::remove_dir_all(p) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                clear_readonly_tree(p);
                std::fs::remove_dir_all(p)
            }
            r => r,
        }
    } else if meta.is_dir() {
        // A junction / directory symlink: remove the link, never what it points to.
        std::fs::remove_dir(p)
    } else {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                clear_readonly(p);
                std::fs::remove_file(p)
            }
            r => r,
        }
    }
}

fn clear_readonly(p: &Path) {
    if let Ok(m) = std::fs::metadata(p) {
        let mut perm = m.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(p, perm);
        }
    }
}

fn clear_readonly_tree(p: &Path) {
    clear_readonly(p);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                clear_readonly_tree(&path);
            } else {
                clear_readonly(&path);
            }
        }
    }
}

/// The message without std's ` (os error 5)` tail.
pub fn os_message(e: &io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// Two passes through temporary names, so `a → b, b → a` works. Stops at the first
/// failure and puts that pass's renames back.
fn bulk_rename(pairs: &[(String, String)]) -> Result<Done, String> {
    let stamp = std::process::id();
    let tmp: Vec<String> = pairs
        .iter()
        .enumerate()
        .map(|(i, (from, _))| format!("{from}.sentier-{stamp}-{i}.tmp"))
        .collect();
    for (i, (from, _)) in pairs.iter().enumerate() {
        if let Err(e) = std::fs::rename(from, &tmp[i]) {
            for j in 0..i {
                let _ = std::fs::rename(&tmp[j], &pairs[j].0);
            }
            return Err(format!("{}: {}", name_of(from), os_message(&e)));
        }
    }
    let mut done = 0;
    let mut failed = Vec::new();
    for (i, (from, to)) in pairs.iter().enumerate() {
        let target_taken = Path::new(to).exists();
        let r = if target_taken {
            Err(format!("{} already exists", name_of(to)))
        } else {
            std::fs::rename(&tmp[i], to).map_err(|e| os_message(&e))
        };
        match r {
            Ok(()) => done += 1,
            Err(e) => {
                // Put it back under its old name.
                let _ = std::fs::rename(&tmp[i], from);
                failed.push(format!("{}: {e}", name_of(from)));
            }
        }
    }
    if failed.is_empty() {
        Ok(Done {
            message: format!("renamed {}", count(done)),
            select: pairs.first().map(|(_, to)| name_of(to).to_string()),
        })
    } else {
        Err(format!(
            "renamed {done}, {} kept their names: {}",
            failed.len(),
            failed.join("; ")
        ))
    }
}

/// Checks a name typed for `r` / `a` / `A`. `a` and `A` allow `sub\name` (made with its
/// parents); `r` doesn't.
pub fn check_name(name: &str, nested: bool) -> Result<(), String> {
    let name = name.trim_end_matches('\\');
    if name.trim().is_empty() {
        return Err("a name, please".into());
    }
    let parts: Vec<&str> = name.split('\\').collect();
    if parts.len() > 1 && !nested {
        return Err("a name, not a path".into());
    }
    for p in parts {
        if p.is_empty() || p == "." || p == ".." {
            return Err(format!("not a valid name: {name}"));
        }
        if let Some(c) = p
            .chars()
            .find(|&c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/') || c.is_control())
        {
            return Err(format!("{c:?} isn't allowed in a name"));
        }
        if p.ends_with(['.', ' ']) {
            return Err("a name can't end in a dot or a space".into());
        }
        let stem = p.split('.').next().unwrap_or(p).to_ascii_uppercase();
        let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0');
        if reserved {
            return Err(format!("{p} is a reserved name on Windows"));
        }
    }
    Ok(())
}

/// Where the rename prompt's cursor starts: before the extension (`report|.pdf`), at the
/// end for folders and names like `.gitignore`.
pub fn rename_cursor(name: &str, is_dir: bool) -> usize {
    if is_dir {
        return name.chars().count();
    }
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].chars().count(),
        _ => name.chars().count(),
    }
}

/// `:rename`: the edited lines against the old names. Lines must match one to one; blank
/// lines, duplicates and bad names are refused. Returns the (old, new) names that change.
pub fn plan_bulk(old: &[String], edited: &str) -> Result<Vec<(String, String)>, String> {
    let lines: Vec<&str> = edited.lines().map(|l| l.trim_end_matches('\r')).collect();
    // An editor may add a final empty line.
    let lines: Vec<&str> = match lines.split_last() {
        Some((last, rest)) if last.is_empty() && rest.len() == old.len() => rest.to_vec(),
        _ => lines,
    };
    if lines.len() != old.len() {
        return Err(format!(
            "{} names in, {} lines back: one line per name, in order",
            old.len(),
            lines.len()
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (o, n) in old.iter().zip(&lines) {
        let n = n.trim();
        check_name(n, false).map_err(|e| format!("line {:?}: {e}", n))?;
        let key = n.to_lowercase();
        if seen.contains(&key) {
            return Err(format!("{n} appears twice"));
        }
        seen.push(key);
        if o != n {
            out.push((o.clone(), n.to_string()));
        }
    }
    // A new name that's an untouched existing name would collide.
    for (_, n) in &out {
        let clash = old
            .iter()
            .any(|o| o.eq_ignore_ascii_case(n) && !out.iter().any(|(oo, _)| oo == o));
        if clash {
            return Err(format!("{n} is already taken"));
        }
    }
    Ok(out)
}

/// One line of `%LOCALAPPDATA%\sentier\audit.log` (admin-share changes).
pub fn audit_line(when: &str, host: &str, user: Option<&str>, op: &FileOp, ok: bool) -> String {
    format!(
        "{when}\t{host}\t{}\t{}\t{}\t{}",
        user.unwrap_or("-"),
        op.describe(),
        if ok { "ok" } else { "FAILED" },
        op.paths().join(" | ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sentier-ops-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    #[test]
    fn names_table() {
        let ok = ["a.txt", "B-1001_rev3.pdf", ".gitignore", "évars é.bat"];
        for n in ok {
            assert_eq!(check_name(n, false), Ok(()), "{n}");
        }
        let bad = [
            "", "  ", "a:b", "a*b", "con", "COM1.txt", "lpt9", "x.", "x ", "..", "a/b", r"a\b",
        ];
        for n in bad {
            assert!(check_name(n, false).is_err(), "{n}");
        }
        assert_eq!(check_name(r"sub\deep\x.txt", true), Ok(()));
        assert_eq!(check_name(r"newdir\", true), Ok(()));
        assert!(check_name(r"a\\b", true).is_err());
        assert_eq!(
            check_name("COM0", false),
            Ok(()),
            "only COM1-9 are reserved"
        );
    }

    #[test]
    fn rename_cursor_before_extension() {
        assert_eq!(rename_cursor("report.pdf", false), 6);
        assert_eq!(rename_cursor("a.tar.gz", false), 5);
        assert_eq!(rename_cursor(".gitignore", false), 10);
        assert_eq!(rename_cursor("README", false), 6);
        assert_eq!(rename_cursor("v1.2", true), 4);
        assert_eq!(rename_cursor("é.txt", false), 1);
    }

    #[test]
    fn bulk_plan_table() {
        let old: Vec<String> = ["a.txt", "b.txt", "c.txt"].map(String::from).to_vec();
        assert_eq!(
            plan_bulk(&old, "a.txt\nB.txt\nd.txt\n"),
            Ok(vec![
                ("b.txt".into(), "B.txt".into()),
                ("c.txt".into(), "d.txt".into())
            ])
        );
        assert_eq!(plan_bulk(&old, "a.txt\r\nb.txt\r\nc.txt\r\n"), Ok(vec![]));
        // Swaps are fine.
        assert_eq!(
            plan_bulk(&old, "b.txt\na.txt\nc.txt").map(|v| v.len()),
            Ok(2)
        );
        assert!(plan_bulk(&old, "a.txt\nb.txt").is_err_and(|e| e.contains("one line per name")));
        assert!(plan_bulk(&old, "a.txt\na.txt\nx.txt").is_err_and(|e| e.contains("twice")));
        assert!(plan_bulk(&old, "a.txt\n\nc.txt").is_err());
        // a → c is fine while c moves on to z.
        assert_eq!(
            plan_bulk(&old, "c.txt\nb.txt\nz.txt").map(|v| v.len()),
            Ok(2)
        );
        // b → a while a stays: taken.
        assert!(plan_bulk(&old, "a.txt\nA.TXT\nc.txt").is_err());
    }

    #[test]
    fn run_ops() {
        let d = temp();
        // New file with parents, then it exists.
        let f = d.join("sub").join("new.txt");
        let r = FileOp::NewFile(s(&f)).run();
        assert_eq!(r.map(|x| x.select), Ok(Some("new.txt".into())));
        assert!(f.exists());
        assert!(
            FileOp::NewFile(s(&f))
                .run()
                .is_err_and(|e| e.contains("already exists"))
        );
        // Folder.
        let dir = d.join("folder");
        assert!(FileOp::Mkdir(s(&dir)).run().is_ok());
        assert!(FileOp::Mkdir(s(&dir)).run().is_err());
        // Rename: refuses an existing target, allows a case-only change.
        std::fs::write(d.join("a.txt"), "a").ok();
        std::fs::write(d.join("b.txt"), "b").ok();
        let rn = |a: &str, b: &str| {
            FileOp::Rename {
                from: s(&d.join(a)),
                to: s(&d.join(b)),
            }
            .run()
        };
        assert!(rn("a.txt", "b.txt").is_err_and(|e| e.contains("already exists")));
        assert!(rn("a.txt", "A.txt").is_ok());
        assert!(rn("A.txt", "c.txt").is_ok());
        assert_eq!(
            std::fs::read_to_string(d.join("c.txt")).ok().as_deref(),
            Some("a")
        );
        // Bulk: a swap.
        let pairs = vec![
            (s(&d.join("b.txt")), s(&d.join("c.txt"))),
            (s(&d.join("c.txt")), s(&d.join("b.txt"))),
        ];
        assert!(FileOp::BulkRename(pairs).run().is_ok());
        assert_eq!(
            std::fs::read_to_string(d.join("b.txt")).ok().as_deref(),
            Some("a")
        );
        assert_eq!(
            std::fs::read_to_string(d.join("c.txt")).ok().as_deref(),
            Some("b")
        );
        // Permanent delete: a read-only file and a tree.
        let ro = d.join("ro.txt");
        std::fs::write(&ro, "x").ok();
        if let Ok(m) = std::fs::metadata(&ro) {
            let mut p = m.permissions();
            p.set_readonly(true);
            let _ = std::fs::set_permissions(&ro, p);
        }
        let r = FileOp::Delete(vec![s(&ro), s(&d.join("sub")), s(&d.join("nope"))]).run();
        assert!(
            r.is_err_and(|e| e.starts_with("1 of 3 not deleted: nope")),
            "the missing one"
        );
        assert!(!ro.exists() && !d.join("sub").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn describe_and_audit() {
        let op = FileOp::Delete(vec![r"\\PC-042\C$\Users\jdoe\Desktop\x.txt".into()]);
        assert_eq!(op.describe(), "delete x.txt permanently");
        let line = audit_line("2026-11-06 10:00:00", "PC-042", Some("jdoe"), &op, true);
        assert_eq!(
            line,
            "2026-11-06 10:00:00\tPC-042\tjdoe\tdelete x.txt permanently\tok\t\\\\PC-042\\C$\\Users\\jdoe\\Desktop\\x.txt"
        );
        assert_eq!(
            FileOp::Trash(vec!["a".into(), "b".into()]).describe(),
            "move 2 items to the Recycle Bin"
        );
    }
}
