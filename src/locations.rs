//! The Locations root's saved shares (CLAUDE.md feature 2).
//!
//! A share is saved the first time a listing under it succeeds, so the list only ever
//! holds places that really existed. It lives in `%APPDATA%\sentier\shares.txt`, one
//! `\\server\share` per line, so a hand edit is trivial. Pinned folders join the root
//! in M3 through `pins.toml`; this file stays the store for the SHARES section.
//!
//! [`load`] and [`save`] block; only the worker calls them.

use crate::fs::path::{FsPath, Loc};
use std::io;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "shares.txt";
const HEADER: &str = "# sentier saved shares: one \\\\server\\share per line.\n\
                      # Added when you first browse a share; `dd` in Locations removes one.\n";

/// `%APPDATA%\sentier\shares.txt`, or None when APPDATA isn't set.
pub fn store_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("sentier").join(FILE_NAME))
}

/// Parses the store. Blank lines and `#` comments are skipped, and so is anything that
/// isn't a share root (logged, not fatal: a hand edit must never stop the app).
/// Duplicates (case-insensitive) keep their first position.
pub fn parse(text: &str) -> Vec<FsPath> {
    let mut out: Vec<FsPath> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match Loc::parse(line) {
            Ok(Loc::Fs(p)) if p.is_network() && p.is_volume_root() => {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
            other => tracing::warn!(line, ?other, "shares.txt: not a \\\\server\\share root"),
        }
    }
    out
}

pub fn serialize(shares: &[FsPath]) -> String {
    let mut s = HEADER.to_string();
    for p in shares {
        s.push_str(&p.display());
        s.push('\n');
    }
    s
}

/// Reads the store. A missing file is an empty list.
pub fn load(path: &Path) -> io::Result<Vec<FsPath>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Writes the store atomically: a temp file next to it, then a rename over it, so a
/// crash mid-write never leaves a truncated list.
pub fn save(path: &Path, shares: &[FsPath]) -> io::Result<()> {
    crate::fs::write_atomic(path, &serialize(shares))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shares(text: &str) -> Vec<String> {
        parse(text).iter().map(FsPath::display).collect()
    }

    #[test]
    fn parse_table() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("\n  \n# comment\n", &[]),
            (r"\\srv\project", &[r"\\srv\project"]),
            ("  \\\\srv\\project\\  \r\n", &[r"\\srv\project"]),
            ("//srv/project", &[r"\\srv\project"]),
            (r"\\?\UNC\srv\project", &[r"\\srv\project"]),
            // duplicates, case-insensitively, keep the first spelling
            ("\\\\srv\\Project\n\\\\SRV\\project", &[r"\\srv\Project"]),
            // order is kept
            ("\\\\b\\x\n\\\\a\\y", &[r"\\b\x", r"\\a\y"]),
            // not share roots: skipped
            (r"C:\dir", &[]),
            (r"\\srv", &[]),
            (r"\\srv\share\deeper", &[]),
            ("garbage", &[]),
        ];
        for (text, want) in cases {
            assert_eq!(shares(text), *want, "input: {text:?}");
        }
    }

    #[test]
    fn serialize_round_trips() {
        let list = parse("\\\\srv01\\aveva\n\\\\nas02\\aveva_archive\n");
        let text = serialize(&list);
        assert!(text.starts_with('#'), "has a header comment");
        assert_eq!(parse(&text), list);
    }

    #[test]
    fn load_and_save() {
        let dir = std::env::temp_dir().join(format!("sentier-test-{}", std::process::id()));
        let path = dir.join("nested").join(FILE_NAME);
        assert_eq!(
            load(&path).ok().map(|v| v.len()),
            Some(0),
            "missing = empty"
        );
        let list = parse(r"\\srv\a");
        assert!(save(&path, &list).is_ok());
        assert_eq!(load(&path).ok(), Some(list));
        assert!(
            !path.with_extension("txt.tmp").exists(),
            "temp file renamed away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
