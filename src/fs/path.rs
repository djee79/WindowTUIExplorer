//! Path normalization and parent/child logic (CLAUDE.md rule 3).
//!
//! Every location the explorer can show is a [`Loc`]:
//!
//! ```text
//!   Locations root            Loc::Root
//!   └── \\server              Loc::Server        (lists that server's shares)
//!       └── \\server\share    Loc::Fs(Share)
//!   └── C:\                   Loc::Fs(Drive)
//! ```
//!
//! Paths are parsed into components once, here, and are never split on `\` anywhere else.
//! Parsing is pure string work and runs on any thread; it never touches the filesystem.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// Above this many UTF-16 units, the OS form gets a `\\?\` / `\\?\UNC\` prefix.
///
/// MAX_PATH is 260 including the terminating NUL, and `CreateDirectoryW` stops at 248
/// (room for an 8.3 name). Prefixing from 248 keeps every Win32 call on the safe side.
const LONG_PATH_THRESHOLD: usize = 247;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("empty path")]
    Empty,
    #[error("relative path needs a base directory: {0}")]
    Relative(String),
    #[error("unsupported path form: {0}")]
    Unsupported(String),
    #[error("invalid path: {0}")]
    Invalid(String),
}

/// The root of a real filesystem path.
#[derive(Debug, Clone)]
pub enum FsRoot {
    /// `C:\`. The letter is stored uppercase.
    Drive(char),
    /// `\\server\share`.
    Share { server: String, share: String },
}

/// A normalized, absolute filesystem path: a root plus clean components
/// (no empty parts, no `.` or `..`).
#[derive(Debug, Clone)]
pub struct FsPath {
    root: FsRoot,
    parts: Vec<String>,
}

/// Anything the explorer can list.
#[derive(Debug, Clone)]
pub enum Loc {
    /// The virtual Locations root (drives, pins, shares).
    Root,
    /// A virtual server node, `\\server`, listing its shares.
    Server(String),
    /// A real directory or file.
    Fs(FsPath),
    /// Search results shown as a folder (`s` / `S`). Entries are named by their path
    /// relative to `origin`, the directory the search ran from, which is also the parent.
    Results { id: u64, origin: Box<Loc> },
}

impl FsPath {
    pub fn root(&self) -> &FsRoot {
        &self.root
    }

    /// `C:\` or `\\server\share`: the volume this path lives on.
    pub fn volume_root(&self) -> FsPath {
        FsPath {
            root: self.root.clone(),
            parts: Vec::new(),
        }
    }

    pub fn is_network(&self) -> bool {
        matches!(self.root, FsRoot::Share { .. })
    }

    /// The server of a `\\server\share` path.
    pub fn server(&self) -> Option<&str> {
        match &self.root {
            FsRoot::Share { server, .. } => Some(server),
            FsRoot::Drive(_) => None,
        }
    }

    /// The (uppercase) letter of a drive path.
    pub fn drive(&self) -> Option<char> {
        match self.root {
            FsRoot::Drive(c) => Some(c),
            FsRoot::Share { .. } => None,
        }
    }

    /// `C:\` or `\\server\share` itself, with no components below it.
    pub fn is_volume_root(&self) -> bool {
        self.parts.is_empty()
    }

    /// Display form: `C:\`, `C:\dir`, `\\server\share`, `\\server\share\dir`. Never `\\?\`.
    pub fn display(&self) -> String {
        let mut s = match &self.root {
            FsRoot::Drive(l) => format!("{l}:\\"),
            FsRoot::Share { server, share } => format!("\\\\{server}\\{share}"),
        };
        for (i, p) in self.parts.iter().enumerate() {
            let needs_sep = !(i == 0 && matches!(self.root, FsRoot::Drive(_)));
            if needs_sep {
                s.push('\\');
            }
            s.push_str(p);
        }
        s
    }

    /// The form handed to the OS. Adds the verbatim prefix for long paths.
    pub fn os_string(&self) -> String {
        let display = self.display();
        if display.encode_utf16().count() <= LONG_PATH_THRESHOLD {
            // A share root as `\\server\share\` is accepted by every API; without the
            // trailing separator some (GetDiskFreeSpaceExW) refuse it.
            return match (&self.root, self.parts.is_empty()) {
                (FsRoot::Share { .. }, true) => format!("{display}\\"),
                _ => display,
            };
        }
        match &self.root {
            FsRoot::Drive(_) => format!("\\\\?\\{display}"),
            // `\\server\share\x` → `\\?\UNC\server\share\x`
            FsRoot::Share { .. } => format!("\\\\?\\UNC\\{}", &display[2..]),
        }
    }

    pub fn to_os_path(&self) -> PathBuf {
        PathBuf::from(self.os_string())
    }

    /// Last component, or `C:` for a drive root, or the share name for a share root.
    pub fn name(&self) -> String {
        match (self.parts.last(), &self.root) {
            (Some(p), _) => p.clone(),
            (None, FsRoot::Drive(l)) => format!("{l}:"),
            (None, FsRoot::Share { share, .. }) => share.clone(),
        }
    }

    /// Same root and `self`'s components start with `base`'s (case-insensitive).
    pub fn starts_with(&self, base: &FsPath) -> bool {
        let same_root = match (&self.root, &base.root) {
            (FsRoot::Drive(a), FsRoot::Drive(b)) => a == b,
            (
                FsRoot::Share {
                    server: s1,
                    share: h1,
                },
                FsRoot::Share {
                    server: s2,
                    share: h2,
                },
            ) => s1.eq_ignore_ascii_case(s2) && h1.eq_ignore_ascii_case(h2),
            _ => false,
        };
        same_root
            && self.parts.len() >= base.parts.len()
            && self
                .parts
                .iter()
                .zip(&base.parts)
                .all(|(a, b)| a.to_lowercase() == b.to_lowercase())
    }

    /// Appends one component. Rejects separators, `.`/`..` and invalid characters.
    pub fn join(&self, name: &str) -> Result<FsPath, PathError> {
        validate_component(name)?;
        if name == "." || name == ".." || name.contains(['\\', '/']) {
            return Err(PathError::Invalid(format!("not a single name: {name}")));
        }
        let mut parts = self.parts.clone();
        parts.push(name.to_string());
        Ok(FsPath {
            root: self.root.clone(),
            parts,
        })
    }

    fn key(&self) -> String {
        self.display().to_lowercase()
    }
}

impl Loc {
    /// Parses an absolute path in any form from rule 3.
    pub fn parse(input: &str) -> Result<Loc, PathError> {
        let s = clean_input(input);
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        let s = s.replace('/', "\\");

        if let Some(rest) = strip_prefix_ci(&s, r"\\?\UNC\") {
            return parse_unc(rest);
        }
        if let Some(rest) = s.strip_prefix(r"\\?\").or_else(|| s.strip_prefix(r"\??\")) {
            return match split_drive(rest) {
                Some((letter, tail)) if tail.is_empty() || tail.starts_with('\\') => {
                    build_drive(letter, tail)
                }
                _ => Err(PathError::Unsupported(input.to_string())),
            };
        }
        if s.starts_with(r"\\.\") {
            return Err(PathError::Unsupported(input.to_string()));
        }
        if let Some(rest) = s.strip_prefix(r"\\") {
            return parse_unc(rest);
        }
        if let Some((letter, tail)) = split_drive(&s) {
            if tail.is_empty() || tail.starts_with('\\') {
                return build_drive(letter, tail);
            }
            // `C:foo` is relative to the current dir of drive C, a per-process concept
            // we don't support.
            return Err(PathError::Unsupported(input.to_string()));
        }
        Err(PathError::Relative(input.to_string()))
    }

    /// Like [`Loc::parse`], but resolves relative input against `base`:
    /// `foo\bar` and `..\x` are relative to `base`, `\foo` is relative to `base`'s root.
    pub fn resolve(base: &Loc, input: &str) -> Result<Loc, PathError> {
        match Loc::parse(input) {
            Err(PathError::Relative(_)) => {}
            other => return other,
        }
        let s = clean_input(input).replace('/', "\\");
        let Loc::Fs(base) = base else {
            return Err(PathError::Relative(input.to_string()));
        };
        let start = if s.starts_with('\\') {
            Vec::new()
        } else {
            base.parts.clone()
        };
        let parts = normalize_parts(start, &s)?;
        Ok(Loc::Fs(FsPath {
            root: base.root.clone(),
            parts,
        }))
    }

    /// The parent location. Never computed by string splitting:
    /// `C:\` → Root, `\\srv\share` → `\\srv`, `\\srv` → Root, Root → None.
    pub fn parent(&self) -> Option<Loc> {
        match self {
            Loc::Root => None,
            Loc::Results { origin, .. } => Some((**origin).clone()),
            Loc::Server(_) => Some(Loc::Root),
            Loc::Fs(p) if p.parts.is_empty() => match &p.root {
                FsRoot::Drive(_) => Some(Loc::Root),
                FsRoot::Share { server, .. } => Some(Loc::Server(server.clone())),
            },
            Loc::Fs(p) => {
                let mut parts = p.parts.clone();
                parts.pop();
                Some(Loc::Fs(FsPath {
                    root: p.root.clone(),
                    parts,
                }))
            }
        }
    }

    /// The child named `name` as it appears in this location's listing.
    ///
    /// The Locations root lists drives by letter (`C:`), and saved shares and pinned
    /// folders by their full path (`\\server\share`, `D:\aveva\local\HIT`), so each joins
    /// back to the location it names.
    pub fn join(&self, name: &str) -> Result<Loc, PathError> {
        match self {
            Loc::Root => match split_drive(name) {
                Some((letter, tail)) if tail.is_empty() || tail == "\\" => build_drive(letter, ""),
                _ if name.starts_with(r"\\") || split_drive(name).is_some() => {
                    match Loc::parse(name)? {
                        loc @ (Loc::Fs(_) | Loc::Server(_)) => Ok(loc),
                        _ => Err(PathError::Invalid(format!("not a location: {name}"))),
                    }
                }
                _ => Err(PathError::Invalid(format!("not a location: {name}"))),
            },
            Loc::Server(server) => {
                validate_component(name)?;
                if name.is_empty() || name.contains(['\\', '/']) {
                    return Err(PathError::Invalid(format!("not a share name: {name}")));
                }
                Ok(Loc::Fs(FsPath {
                    root: FsRoot::Share {
                        server: server.clone(),
                        share: name.to_string(),
                    },
                    parts: Vec::new(),
                }))
            }
            Loc::Fs(p) => p.join(name).map(Loc::Fs),
            // A result's name is its path relative to the search root.
            Loc::Results { origin, .. } => match Loc::resolve(origin, name)? {
                Loc::Fs(p) if origin.as_fs().is_some_and(|o| p.starts_with(o)) => Ok(Loc::Fs(p)),
                _ => Err(PathError::Invalid(format!(
                    "not inside the search root: {name}"
                ))),
            },
        }
    }

    /// The name this location has inside its parent's listing.
    pub fn name(&self) -> String {
        match self {
            Loc::Root => "Locations".to_string(),
            Loc::Server(s) => s.clone(),
            Loc::Fs(p) => p.name(),
            Loc::Results { .. } => "search".to_string(),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Loc::Root => "Locations".to_string(),
            Loc::Server(s) => format!("\\\\{s}"),
            Loc::Fs(p) => p.display(),
            Loc::Results { origin, .. } => format!("search in {}", origin.display()),
        }
    }

    /// The real directory behind this location: itself, or a search's origin.
    pub fn real(&self) -> &Loc {
        match self {
            Loc::Results { origin, .. } => origin.real(),
            other => other,
        }
    }

    /// True for anything that lives on another machine as far as the path can tell.
    /// (Mapped drive letters look local here; the worker asks the OS about those.)
    pub fn is_network(&self) -> bool {
        match self {
            Loc::Root => false,
            Loc::Server(_) => true,
            Loc::Fs(p) => p.is_network(),
            Loc::Results { origin, .. } => origin.is_network(),
        }
    }

    pub fn as_fs(&self) -> Option<&FsPath> {
        match self {
            Loc::Fs(p) => Some(p),
            _ => None,
        }
    }

    fn key(&self) -> String {
        match self {
            Loc::Root => String::new(),
            Loc::Server(s) => format!("\\\\{}", s.to_lowercase()),
            Loc::Fs(p) => p.key(),
            // `|` can't appear in a path, so this never collides with a real one.
            Loc::Results { id, .. } => format!("|search|{id}"),
        }
    }
}

// Windows paths compare case-insensitively.
impl PartialEq for FsPath {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for FsPath {}
impl PartialEq for Loc {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for Loc {}
impl Hash for Loc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl fmt::Display for Loc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Splits typed path text for Tab completion into the directory part and the name prefix
/// being completed: `C:\wo` → (`C:\`, `wo`), `\\srv\` → (`\\srv\`, ``), `D:` → (`D:`, ``).
///
/// This is text editing on what the user typed, not path logic: the directory part is
/// then resolved with [`Loc::resolve`] like any other input.
pub fn split_for_completion(input: &str) -> (&str, &str) {
    if let Some(i) = input.rfind(['\\', '/']) {
        return (&input[..=i], &input[i + 1..]);
    }
    if split_drive(input).is_some_and(|(_, tail)| tail.is_empty()) {
        return (input, "");
    }
    ("", input)
}

/// Trims whitespace and the quotes Explorer's "Copy as path" adds.
fn clean_input(input: &str) -> &str {
    let s = input.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
        .trim()
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// `C:rest` → ('C', "rest").
fn split_drive(s: &str) -> Option<(char, &str)> {
    let mut chars = s.chars();
    let letter = chars.next()?;
    if letter.is_ascii_alphabetic() && chars.next() == Some(':') {
        Some((letter.to_ascii_uppercase(), &s[2..]))
    } else {
        None
    }
}

fn build_drive(letter: char, tail: &str) -> Result<Loc, PathError> {
    Ok(Loc::Fs(FsPath {
        root: FsRoot::Drive(letter),
        parts: normalize_parts(Vec::new(), tail)?,
    }))
}

/// `server[\share[\rest]]` (the part after `\\` or `\\?\UNC\`).
fn parse_unc(rest: &str) -> Result<Loc, PathError> {
    let mut it = rest.split('\\').filter(|p| !p.is_empty());
    let Some(server) = it.next() else {
        return Err(PathError::Invalid("missing server name".to_string()));
    };
    validate_component(server)?;
    if server == "." || server == ".." {
        return Err(PathError::Invalid(format!("bad server name: {server}")));
    }
    let Some(share) = it.next() else {
        return Ok(Loc::Server(server.to_string()));
    };
    validate_component(share)?;
    if share == "." || share == ".." {
        return Err(PathError::Invalid(format!("bad share name: {share}")));
    }
    let tail: Vec<&str> = it.collect();
    Ok(Loc::Fs(FsPath {
        root: FsRoot::Share {
            server: server.to_string(),
            share: share.to_string(),
        },
        parts: normalize_parts(Vec::new(), &tail.join("\\"))?,
    }))
}

/// Applies `tail` (separated by `\`) onto `parts`: drops empty parts and `.`,
/// pops on `..` (clamped at the root, like Win32 does).
fn normalize_parts(mut parts: Vec<String>, tail: &str) -> Result<Vec<String>, PathError> {
    for p in tail.split('\\') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => {
                validate_component(p)?;
                parts.push(p.to_string());
            }
        }
    }
    Ok(parts)
}

fn validate_component(p: &str) -> Result<(), PathError> {
    let bad = |c: char| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control();
    if let Some(c) = p.chars().find(|&c| bad(c)) {
        return Err(PathError::Invalid(format!(
            "character {c:?} not allowed in {p:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> String {
        match Loc::parse(s) {
            Ok(l) => l.display(),
            Err(e) => format!("ERR {e:?}"),
        }
    }

    #[test]
    fn parse_display_table() {
        let cases: &[(&str, &str)] = &[
            // drives
            (r"C:\", r"C:\"),
            (r"c:\", r"C:\"),
            (r"C:", r"C:\"),
            (r"C:\dir", r"C:\dir"),
            (r"C:\dir\", r"C:\dir"),
            (r"C:\a\\b", r"C:\a\b"),
            (r"C:\a\.\b", r"C:\a\b"),
            (r"C:\a\..\b", r"C:\b"),
            (r"C:\..\..", r"C:\"),
            ("C:/dir/sub", r"C:\dir\sub"),
            ("  \"C:\\Program Files\"  ", r"C:\Program Files"),
            // verbatim drive
            (r"\\?\C:\", r"C:\"),
            (r"\\?\C:\dir\x", r"C:\dir\x"),
            (r"\??\D:\x", r"D:\x"),
            // UNC
            (r"\\server\share", r"\\server\share"),
            (r"\\server\share\", r"\\server\share"),
            (r"\\server\share\dir", r"\\server\share\dir"),
            (r"\\server\share\dir\..\..", r"\\server\share"),
            ("//server/share/dir", r"\\server\share\dir"),
            (r"\\10.255.255.1\x", r"\\10.255.255.1\x"),
            // hidden share (`$`): typed by hand, never mapped
            (r"\\srv01\STD$", r"\\srv01\STD$"),
            (
                r"\\srv01\STD$\cat\",
                r"\\srv01\STD$\cat",
            ),
            // verbatim UNC
            (r"\\?\UNC\server\share", r"\\server\share"),
            (r"\\?\unc\server\share\dir", r"\\server\share\dir"),
            ("//?/UNC/server/share/a", r"\\server\share\a"),
            // server node
            (r"\\server", r"\\server"),
            (r"\\server\", r"\\server"),
            (r"\\?\UNC\server", r"\\server"),
            // errors
            ("", "ERR Empty"),
            ("   ", "ERR Empty"),
            ("dir", "ERR Relative(\"dir\")"),
            (r"\dir", "ERR Relative(\"\\\\dir\")"),
            ("C:dir", "ERR Unsupported(\"C:dir\")"),
            (
                r"\\.\PhysicalDrive0",
                "ERR Unsupported(\"\\\\\\\\.\\\\PhysicalDrive0\")",
            ),
            (
                r"\\?\Volume{abc}\x",
                "ERR Unsupported(\"\\\\\\\\?\\\\Volume{abc}\\\\x\")",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(&d(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn invalid_inputs() {
        for input in [
            r"\\",
            r"\\\\",
            r"C:\a|b",
            r"C:\a*",
            r"\\srv\sh\x?y",
            r"\\..\x",
        ] {
            assert!(
                matches!(Loc::parse(input), Err(PathError::Invalid(_))),
                "input: {input:?} gave {:?}",
                Loc::parse(input)
            );
        }
    }

    #[test]
    fn parent_chain() {
        let cases: &[(&str, &[&str])] = &[
            (r"C:\a\b", &[r"C:\a", r"C:\", "Locations"]),
            (r"\\srv\share\a", &[r"\\srv\share", r"\\srv", "Locations"]),
            (r"\\srv\STD$\a", &[r"\\srv\STD$", r"\\srv", "Locations"]),
            (r"\\?\UNC\srv\share", &[r"\\srv", "Locations"]),
            (r"\\srv", &["Locations"]),
        ];
        for (start, chain) in cases {
            let mut loc = Loc::parse(start).ok();
            for want in *chain {
                loc = loc.and_then(|l| l.parent());
                assert_eq!(
                    loc.as_ref().map(Loc::display).as_deref(),
                    Some(*want),
                    "from {start}"
                );
            }
            assert_eq!(loc.and_then(|l| l.parent()), None, "Root has no parent");
        }
    }

    #[test]
    fn name_and_join_round_trip() {
        // `\\srv` is not here: the Locations root lists drives by name, while servers
        // will appear there as saved entries carrying their own target (M2).
        let cases: &[&str] = &[
            r"C:\",
            r"C:\a\b",
            r"\\srv\share",
            r"\\srv\share\x",
            r"\\srv\STD$",
        ];
        for start in cases {
            let Ok(loc) = Loc::parse(start) else {
                panic!("parse {start}")
            };
            let parent = loc.parent().expect("has parent");
            let back = parent.join(&loc.name()).expect("join");
            assert_eq!(back, loc, "{start}");
        }
    }

    #[test]
    fn names() {
        let cases: &[(&str, &str)] = &[
            (r"C:\", "C:"),
            (r"C:\a\Bee", "Bee"),
            (r"\\srv\share", "share"),
            (r"\\srv", "srv"),
        ];
        for (input, want) in cases {
            assert_eq!(Loc::parse(input).map(|l| l.name()).as_deref(), Ok(*want));
        }
        assert_eq!(Loc::Root.name(), "Locations");
    }

    #[test]
    fn join_rejects_non_names() {
        let Ok(c) = Loc::parse(r"C:\x") else { panic!() };
        for bad in ["a\\b", "a/b", "..", ".", "a:b", "a?"] {
            assert!(c.join(bad).is_err(), "{bad}");
        }
        assert!(Loc::Root.join("nope").is_err());
        // Pinned folders are listed by their full path.
        for full in [r"C:\work\HIT", r"\\srv\STD$\cat", r"\\srv\share", r"\\srv"] {
            assert_eq!(
                Loc::Root.join(full).map(|l| l.display()).as_deref(),
                Ok(full),
                "{full}"
            );
        }
        assert!(Loc::Root.join(r"\dir").is_err());
        assert_eq!(
            Loc::Root.join("d:").map(|l| l.display()),
            Ok(r"D:\".to_string())
        );
    }

    #[test]
    fn results_location() {
        let origin = Loc::parse(r"\\srv\project\HIT").expect("origin");
        let res = Loc::Results {
            id: 7,
            origin: Box::new(origin.clone()),
        };
        assert_eq!(res.parent(), Some(origin.clone()), "parent is the origin");
        assert_eq!(res.real(), &origin);
        assert!(res.is_network());
        assert!(res.as_fs().is_none());
        assert_ne!(res, origin);
        assert_eq!(
            res,
            Loc::Results {
                id: 7,
                origin: Box::new(Loc::Root)
            },
            "identity is the id"
        );
        let cases: &[(&str, Option<&str>)] = &[
            (
                r"hitiso\B-1001_rev3.pdf",
                Some(r"\\srv\project\HIT\hitiso\B-1001_rev3.pdf"),
            ),
            ("evars.bat", Some(r"\\srv\project\HIT\evars.bat")),
            (r"..\escape.txt", None),
            (r"C:\abs", None),
        ];
        for (name, want) in cases {
            assert_eq!(
                res.join(name).ok().map(|l| l.display()).as_deref(),
                *want,
                "{name}"
            );
        }
    }

    #[test]
    fn starts_with_table() {
        let p = |s: &str| Loc::parse(s).ok().and_then(|l| l.as_fs().cloned());
        let cases: &[(&str, &str, bool)] = &[
            (r"C:\a\b", r"C:\a", true),
            (r"C:\a", r"C:\a", true),
            (r"C:\A\b", r"c:\a", true),
            (r"C:\ab", r"C:\a", false),
            (r"C:\a", r"C:\a\b", false),
            (r"D:\a", r"C:\a", false),
            (r"\\srv\s\x", r"\\SRV\S", true),
            (r"\\srv\s\x", r"\\srv\t", false),
        ];
        for (a, b, want) in cases {
            let (Some(a1), Some(b1)) = (p(a), p(b)) else {
                panic!("{a} {b}")
            };
            assert_eq!(a1.starts_with(&b1), *want, "{a} starts_with {b}");
        }
    }

    #[test]
    fn completion_split_table() {
        let cases: &[(&str, (&str, &str))] = &[
            ("", ("", "")),
            ("wo", ("", "wo")),
            (r"C:\wo", (r"C:\", "wo")),
            (r"C:\work\", (r"C:\work\", "")),
            ("C:/a/b", ("C:/a/", "b")),
            (r"\\srv\", (r"\\srv\", "")),
            (r"\\srv\pro", (r"\\srv\", "pro")),
            ("D:", ("D:", "")),
            (r"..\x", (r"..\", "x")),
            ("~", ("", "~")),
        ];
        for (input, want) in cases {
            assert_eq!(split_for_completion(input), *want, "{input:?}");
        }
    }

    #[test]
    fn root_joins_saved_shares_by_full_root() {
        let ok: &[(&str, &str)] = &[
            (r"\\srv\share", r"\\srv\share"),
            (r"\\SRV\Share\", r"\\SRV\Share"),
        ];
        for (name, want) in ok {
            assert_eq!(
                Loc::Root.join(name).map(|l| l.display()).as_deref(),
                Ok(*want)
            );
        }
        // Full paths (pinned folders) join too; relative names and garbage don't.
        for bad in [r"\\", "dir", r"\dir", r"C:dir"] {
            assert!(Loc::Root.join(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolve_table() {
        let base_c = Loc::parse(r"C:\work\proj").expect("base");
        let base_unc = Loc::parse(r"\\srv\share\a\b").expect("base");
        let cases: &[(&Loc, &str, &str)] = &[
            (&base_c, "sub", r"C:\work\proj\sub"),
            (&base_c, r"..\other", r"C:\work\other"),
            (&base_c, r"\root", r"C:\root"),
            (&base_c, r"D:\abs", r"D:\abs"),
            (&base_c, "../../../..", r"C:\"),
            (&base_unc, "..", r"\\srv\share\a"),
            (&base_unc, r"\top", r"\\srv\share\top"),
            (&base_unc, r"\\other\s", r"\\other\s"),
        ];
        for (base, input, want) in cases {
            let got = Loc::resolve(base, input).map(|l| l.display());
            assert_eq!(got.as_deref(), Ok(*want), "{base} + {input}");
        }
        assert!(matches!(
            Loc::resolve(&Loc::Root, "x"),
            Err(PathError::Relative(_))
        ));
    }

    #[test]
    fn case_insensitive_equality() {
        let a = Loc::parse(r"C:\Program Files").expect("a");
        let b = Loc::parse(r"c:\PROGRAM FILES\").expect("b");
        assert_eq!(a, b);
        assert_eq!(Loc::parse(r"\\SRV"), Loc::parse(r"\\srv"));
        assert_ne!(Loc::Root, Loc::parse(r"C:\").expect("c"));
    }

    #[test]
    fn os_form_prefixes_long_paths_only() {
        let short = Loc::parse(r"C:\dir").expect("short");
        assert_eq!(
            short.as_fs().map(FsPath::os_string).as_deref(),
            Some(r"C:\dir")
        );

        let share = Loc::parse(r"\\srv\share").expect("share");
        assert_eq!(
            share.as_fs().map(FsPath::os_string).as_deref(),
            Some(r"\\srv\share\")
        );

        let long_tail = "x".repeat(300);
        let long_c = Loc::parse(&format!(r"C:\{long_tail}")).expect("long c");
        assert_eq!(
            long_c.as_fs().map(FsPath::os_string),
            Some(format!(r"\\?\C:\{long_tail}"))
        );
        assert_eq!(
            long_c.display(),
            format!(r"C:\{long_tail}"),
            "display never shows \\\\?\\"
        );

        let long_unc = Loc::parse(&format!(r"\\srv\share\{long_tail}")).expect("long unc");
        assert_eq!(
            long_unc.as_fs().map(FsPath::os_string),
            Some(format!(r"\\?\UNC\srv\share\{long_tail}"))
        );
        // Round trip: the OS form parses back to the same location.
        let os = long_unc.as_fs().map(FsPath::os_string).unwrap_or_default();
        assert_eq!(Loc::parse(&os), Ok(long_unc));
    }

    #[test]
    fn network_detection() {
        assert!(Loc::parse(r"\\srv\s\x").expect("unc").is_network());
        assert!(Loc::parse(r"\\srv").expect("srv").is_network());
        assert!(!Loc::parse(r"C:\x").expect("c").is_network());
        assert!(!Loc::Root.is_network());
    }
}
