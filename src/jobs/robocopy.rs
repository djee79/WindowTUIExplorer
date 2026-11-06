//! The robocopy backend's pure half (CLAUDE.md section 4): the arguments, an incremental
//! parser for its stdout, a tracker that turns the parsed lines into job progress, and
//! the exit-code bitmask in words. Spawning lives in `fs/worker.rs`.
//!
//! The parser leans on structure, not words, because robocopy's labels are translated on
//! non-English Windows: a file line is `\t<class>\t\t<size>\t<path>`, a directory line ends
//! in `\`, progress is a `\r`-separated `NN.N%`, an error is
//! `YYYY/MM/DD HH:MM:SS <word> <n> (0x<hex>) <what> <path>` followed by the message line,
//! and the summary is three rows of six numbers (dirs, files, bytes) in that order. Only
//! the file classes (`New File`, `Newer`, `*EXTRA File`…) are matched as English words, with
//! "copy" as the fallback, and `*EXTRA` keeps its asterisk in every language.
//!
//! A retry is the same file starting again after an error; a file fails when another one
//! starts (or the output ends) while its last attempt had an error. So the English
//! "Waiting 30 seconds... Retrying..." lines are never needed.

use std::sync::LazyLock;

use regex::Regex;

/// What a file line says will happen to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileClass {
    New,
    Newer,
    Older,
    Changed,
    Tweaked,
    Same,
    Mismatch,
    /// Only in the destination: deleted by `/MIR`, listed by `/L`.
    Extra,
    /// A translated or unknown label: copied.
    Other(String),
}

impl FileClass {
    fn parse(label: &str) -> FileClass {
        let l = label.trim();
        if l.starts_with("*EXTRA") || l.starts_with('*') && l.to_ascii_uppercase().contains("EXTRA")
        {
            return FileClass::Extra;
        }
        match l.to_ascii_lowercase().as_str() {
            "new file" => FileClass::New,
            "newer" => FileClass::Newer,
            "older" => FileClass::Older,
            "changed" => FileClass::Changed,
            "tweaked" => FileClass::Tweaked,
            "same" => FileClass::Same,
            "*mismatch" | "mismatch" => FileClass::Mismatch,
            _ => FileClass::Other(l.to_string()),
        }
    }
}

/// One row of the summary table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Row {
    pub total: u64,
    pub copied: u64,
    pub skipped: u64,
    pub mismatch: u64,
    pub failed: u64,
    pub extras: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub dirs: Row,
    pub files: Row,
    pub bytes: Row,
}

/// A robocopy error line and its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcError {
    pub code: u32,
    /// `Copying File`, `Accessing Destination Directory`… (may be translated).
    pub action: String,
    pub path: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Dir {
        path: String,
        extra: bool,
    },
    File {
        class: FileClass,
        size: u64,
        path: String,
    },
    /// Percent of the current file.
    Progress(f32),
    Error(RcError),
    Summary(Summary),
}

static ERROR_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^\d{4}/\d{2}/\d{2} \d{2}:\d{2}:\d{2} \S+ (\d+) \(0x[0-9A-Fa-f]+\) (.*)$").ok()
});
static ROW_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^\s*\S[^:]*:\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s*$").ok()
});
static PCT_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^\s*(\d{1,3}(?:[.,]\d+)?)%\s*$").ok());

/// Feeds on stdout chunks of any size; lines are cut at `\r` and `\n`.
#[derive(Debug, Default)]
pub struct Parser {
    buf: String,
    /// An error line waiting for its message line.
    pending: Option<RcError>,
    rows: Vec<Row>,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    pub fn feed(&mut self, chunk: &str) -> Vec<Event> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find(['\r', '\n']) {
            let frag: String = self.buf[..i].to_string();
            self.buf.drain(..=i);
            self.fragment(&frag, &mut out);
        }
        out
    }

    /// The end of the output: whatever is left in the buffer.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buf);
        self.fragment(&rest, &mut out);
        if let Some(e) = self.pending.take() {
            out.push(Event::Error(e));
        }
        out
    }

    fn fragment(&mut self, frag: &str, out: &mut Vec<Event>) {
        if frag.trim().is_empty() {
            return;
        }
        if let Some(mut e) = self.pending.take() {
            // The line after an error line is its message.
            e.message = frag.trim().to_string();
            out.push(Event::Error(e));
            return;
        }
        if let Some(c) = ERROR_RE.as_ref().and_then(|re| re.captures(frag)) {
            let code = c[1].parse().unwrap_or(0);
            let rest = c[2].trim();
            let (action, path) = split_action(rest);
            self.pending = Some(RcError {
                code,
                action,
                path,
                message: String::new(),
            });
            return;
        }
        if let Some(c) = PCT_RE.as_ref().and_then(|re| re.captures(frag)) {
            let p: f32 = c[1].replace(',', ".").parse().unwrap_or(0.0);
            out.push(Event::Progress(p));
            return;
        }
        if frag.starts_with('\t') {
            if let Some(ev) = item_line(frag) {
                out.push(ev);
            }
            return;
        }
        if let Some(c) = ROW_RE.as_ref().and_then(|re| re.captures(frag)) {
            let n = |i: usize| c[i].parse::<u64>().unwrap_or(0);
            self.rows.push(Row {
                total: n(1),
                copied: n(2),
                skipped: n(3),
                mismatch: n(4),
                failed: n(5),
                extras: n(6),
            });
            if self.rows.len() == 3 {
                out.push(Event::Summary(Summary {
                    dirs: self.rows[0],
                    files: self.rows[1],
                    bytes: self.rows[2],
                }));
            }
        }
    }
}

/// `Copying File C:\x\a.txt` → (`Copying File`, `C:\x\a.txt`): the path starts at the
/// first `\\` or `X:\`.
fn split_action(rest: &str) -> (String, Option<String>) {
    let b = rest.as_bytes();
    let start = (0..b.len()).find(|&i| {
        (b[i] == b'\\' && b.get(i + 1) == Some(&b'\\'))
            || (b[i].is_ascii_alphabetic()
                && b.get(i + 1) == Some(&b':')
                && b.get(i + 2) == Some(&b'\\')
                && (i == 0 || b[i - 1] == b' '))
    });
    match start {
        Some(i) => (rest[..i].trim().to_string(), Some(rest[i..].to_string())),
        None => (rest.to_string(), None),
    }
}

/// A tab-led line: a file (`\t<class>\t\t<size>\t<path>`) or a directory (path ends `\`).
fn item_line(frag: &str) -> Option<Event> {
    let fields: Vec<&str> = frag.split('\t').collect();
    let path = fields.last()?.trim_end().to_string();
    if path.is_empty() {
        return None;
    }
    if path.ends_with('\\') {
        let label = fields[..fields.len() - 1].join(" ");
        return Some(Event::Dir {
            extra: label.contains("*EXTRA") || label.trim_start().starts_with('*'),
            path,
        });
    }
    // The size is the last field before the path; the class the first non-empty one.
    let size_field = fields.get(fields.len().checked_sub(2)?)?.trim();
    let size = parse_size(size_field)?;
    let label = fields[..fields.len() - 2]
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    Some(Event::File {
        class: FileClass::parse(label),
        size,
        path,
    })
}

/// `/BYTES` gives plain integers; without it robocopy writes `1.9 g` and the like.
fn parse_size(s: &str) -> Option<u64> {
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let mut parts = s.split_whitespace();
    let num: f64 = parts.next()?.replace(',', ".").parse().ok()?;
    let mult = match parts.next().map(|u| u.to_ascii_lowercase()) {
        Some(u) if u == "k" => 1024.0,
        Some(u) if u == "m" => 1024.0 * 1024.0,
        Some(u) if u == "g" => 1024.0 * 1024.0 * 1024.0,
        Some(u) if u == "t" => 1024.0_f64.powi(4),
        None => 1.0,
        Some(_) => return None,
    };
    Some((num * mult) as u64)
}

// ------------------------------------------------------------------ the tracker

/// The file being copied.
#[derive(Debug, Clone, PartialEq)]
pub struct Current {
    pub path: String,
    pub size: u64,
    pub pct: f32,
    /// 1 on the first try.
    pub attempt: u32,
    /// The last attempt's error (cleared when it gets to 100 %).
    pub error: Option<RcError>,
}

/// A file that needed more than one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retry {
    pub path: String,
    pub code: u32,
    pub message: String,
    pub attempts: u32,
    /// Some(true) ok in the end, Some(false) gave up, None still trying.
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    pub code: u32,
    pub message: String,
}

/// Progress built from the parser's events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tracker {
    pub current: Option<Current>,
    pub files_done: u64,
    pub bytes_done: u64,
    /// Files listed (`/L`) or removed (`/MIR`) only in the destination, with their size.
    pub extras: Vec<(u64, String)>,
    /// Folders only in the destination (`*EXTRA Dir`).
    pub extra_dirs: Vec<String>,
    /// Files that will be / were copied, with their class (kept for `/L` previews).
    pub listed: Vec<(FileClass, u64, String)>,
    pub retries: Vec<Retry>,
    pub failed: Vec<Failure>,
    /// Errors that aren't about one file (a directory, the destination…), deduplicated.
    pub errors: Vec<RcError>,
    pub summary: Option<Summary>,
    /// `/L`: nothing is copied, so a file line doesn't mean it was done.
    pub list_only: bool,
}

impl Tracker {
    pub fn new(list_only: bool) -> Tracker {
        Tracker {
            list_only,
            ..Tracker::default()
        }
    }

    /// Bytes done including the current file's share.
    pub fn bytes_now(&self) -> u64 {
        let part = self
            .current
            .as_ref()
            .filter(|c| c.pct < 100.0)
            .map_or(0, |c| (c.size as f64 * c.pct as f64 / 100.0) as u64);
        self.bytes_done + part
    }

    /// The current file is on a retry after an error.
    pub fn retrying(&self) -> bool {
        self.current.as_ref().is_some_and(|c| c.error.is_some())
    }

    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::Dir { path, extra } => {
                if extra {
                    self.extra_dirs.push(path);
                }
            }
            Event::File { class, size, path } => {
                if class == FileClass::Extra {
                    self.extras.push((size, path));
                    return;
                }
                if self.list_only {
                    self.listed.push((class, size, path));
                    return;
                }
                let again = self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.path.eq_ignore_ascii_case(&path) && c.pct < 100.0);
                if again {
                    if let Some(c) = self.current.as_mut() {
                        c.attempt += 1;
                        c.pct = 0.0;
                    }
                    return;
                }
                self.settle();
                self.listed.push((class, size, path.clone()));
                self.current = Some(Current {
                    path,
                    size,
                    pct: 0.0,
                    attempt: 1,
                    error: None,
                });
            }
            Event::Progress(p) => {
                let Some(c) = self.current.as_mut() else {
                    return;
                };
                if c.pct >= 100.0 {
                    return;
                }
                c.pct = p.clamp(0.0, 100.0);
                if c.pct >= 100.0 {
                    self.files_done += 1;
                    self.bytes_done += c.size;
                    if c.error.take().is_some()
                        && let Some(r) = self
                            .retries
                            .iter_mut()
                            .rev()
                            .find(|r| r.path.eq_ignore_ascii_case(&c.path))
                    {
                        r.ok = Some(true);
                        r.attempts = c.attempt;
                    }
                }
            }
            Event::Error(e) => {
                let about_current = self.current.as_ref().is_some_and(|c| {
                    c.pct < 100.0
                        && e.path
                            .as_deref()
                            .is_some_and(|p| p.eq_ignore_ascii_case(&c.path))
                });
                if about_current {
                    let Some(c) = self.current.as_mut() else {
                        return;
                    };
                    match self
                        .retries
                        .iter_mut()
                        .rev()
                        .find(|r| r.path.eq_ignore_ascii_case(&c.path) && r.ok.is_none())
                    {
                        Some(r) => {
                            r.attempts = c.attempt;
                            r.code = e.code;
                            r.message = e.message.clone();
                        }
                        None => self.retries.push(Retry {
                            path: c.path.clone(),
                            code: e.code,
                            message: e.message.clone(),
                            attempts: c.attempt,
                            ok: None,
                        }),
                    }
                    c.error = Some(e);
                } else if !self
                    .errors
                    .iter()
                    .any(|o| o.code == e.code && o.path == e.path && o.action == e.action)
                {
                    self.errors.push(e);
                }
            }
            Event::Summary(s) => {
                self.settle();
                self.summary = Some(s);
            }
        }
    }

    /// The current file is over (another started, or the end): failed if its last
    /// attempt had an error; done if it never reported progress (`/MT` can skip it).
    fn settle(&mut self) {
        let Some(c) = self.current.take() else {
            return;
        };
        if c.pct >= 100.0 {
            return;
        }
        match c.error {
            Some(e) => {
                if let Some(r) = self
                    .retries
                    .iter_mut()
                    .rev()
                    .find(|r| r.path.eq_ignore_ascii_case(&c.path))
                {
                    r.ok = Some(false);
                }
                self.failed.push(Failure {
                    path: c.path,
                    code: e.code,
                    message: e.message,
                });
            }
            None => {
                self.files_done += 1;
                self.bytes_done += c.size;
            }
        }
    }

    /// The output ended.
    pub fn finish(&mut self) {
        self.settle();
    }

    /// The file being copied, to show while it is.
    pub fn current_path(&self) -> Option<&str> {
        self.current
            .as_ref()
            .filter(|c| c.pct < 100.0)
            .map(|c| c.path.as_str())
    }
}

// ------------------------------------------------------------------ exit codes

/// What robocopy's exit code (a bitmask) means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitMeaning {
    /// 8 and up: some copies failed, or a fatal error.
    pub failed: bool,
    /// `1 (files copied) + 8 (some copies failed)`.
    pub parts: String,
    /// A short phrase for the job list: `copied`, `nothing to copy`, `failed`…
    pub word: &'static str,
}

pub fn exit_meaning(code: i32) -> ExitMeaning {
    if code < 0 {
        return ExitMeaning {
            failed: true,
            parts: "robocopy didn't exit normally".into(),
            word: "failed",
        };
    }
    let bits: [(i32, &str); 5] = [
        (1, "files copied"),
        (2, "extra files in the destination"),
        (4, "mismatched files"),
        (8, "some copies failed"),
        (16, "fatal error: nothing was copied"),
    ];
    let parts: Vec<String> = bits
        .iter()
        .filter(|(b, _)| code & b != 0)
        .map(|(b, s)| format!("{b} ({s})"))
        .collect();
    let failed = code >= 8;
    let word = if code & 16 != 0 {
        "fatal"
    } else if failed {
        "failed"
    } else if code & 1 != 0 {
        "copied"
    } else if code == 0 {
        "nothing to copy"
    } else {
        "done"
    };
    ExitMeaning {
        failed,
        parts: if parts.is_empty() {
            "0 (nothing to copy: everything was already there)".into()
        } else {
            parts.join(" + ")
        },
        word,
    }
}

// ------------------------------------------------------------------ arguments

/// What to do when a file is already at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Overwrite {
    /// `p`: leave it (skipped, reported).
    Never,
    /// `P`: replace it, even when it looks the same.
    Always,
    /// Sync presets: robocopy's default, replace when it differs.
    Changed,
}

/// One robocopy run: a whole directory, or some files of one directory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub src: String,
    pub dst: String,
    /// Empty: the whole tree (`/E`). Otherwise these file names only.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub moving: bool,
    pub overwrite: Overwrite,
    pub mirror: bool,
    pub list_only: bool,
    /// `/MT:n` (0: off).
    pub threads: u32,
    pub log: Option<String>,
    /// `/XD`: folder names or paths left out (and, with `/MIR`, never deleted).
    pub exclude_dirs: Vec<String>,
    /// `/XF`: file names or globs left out.
    pub exclude_files: Vec<String>,
}

/// `src dst [files] /E|/MIR /Z /R:2 /W:5 /BYTES /FP /TEE [/XD …] [/XF …] [/MOV|/MOVE]
/// [/XC /XN /XO | /IS /IT] [/MT:n] [/L] /UNILOG+:<log>` (a dry run, `/L`, uses `/R:0 /W:0`)
pub fn args(step: &Step, o: &Options) -> Vec<String> {
    let mut a = vec![step.src.clone(), step.dst.clone()];
    a.extend(step.files.iter().cloned());
    if step.files.is_empty() {
        a.push(if o.mirror { "/MIR" } else { "/E" }.into());
    }
    // A dry run touches nothing, so a failure is worth no retries or waits.
    let retries: [&str; 2] = if o.list_only {
        ["/R:0", "/W:0"]
    } else {
        ["/R:2", "/W:5"]
    };
    a.push("/Z".into());
    a.extend(retries.map(String::from));
    for s in ["/BYTES", "/FP", "/TEE"] {
        a.push(s.into());
    }
    if !o.exclude_dirs.is_empty() {
        a.push("/XD".into());
        a.extend(o.exclude_dirs.iter().cloned());
    }
    if !o.exclude_files.is_empty() {
        a.push("/XF".into());
        a.extend(o.exclude_files.iter().cloned());
    }
    if o.moving {
        a.push(
            if step.files.is_empty() {
                "/MOVE"
            } else {
                "/MOV"
            }
            .into(),
        );
    }
    match o.overwrite {
        Overwrite::Never => a.extend(["/XC", "/XN", "/XO"].map(String::from)),
        Overwrite::Always => a.extend(["/IS", "/IT"].map(String::from)),
        Overwrite::Changed => {}
    }
    if o.threads > 0 {
        a.push(format!("/MT:{}", o.threads.min(128)));
    }
    if o.list_only {
        a.push("/L".into());
    }
    if let Some(log) = &o.log {
        a.push(format!("/UNILOG+:{log}"));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (String, i32) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("robocopy");
        let out = std::fs::read(dir.join(format!("{name}.out"))).unwrap_or_default();
        let code = std::fs::read_to_string(dir.join(format!("{name}.exit")))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(-1);
        (String::from_utf8_lossy(&out).to_string(), code)
    }

    /// Feeds the fixture in small chunks (as a pipe would) and tracks it.
    fn track(name: &str, list_only: bool) -> (Tracker, i32) {
        let (text, code) = fixture(name);
        assert!(!text.is_empty(), "fixture {name} missing");
        let mut p = Parser::new();
        let mut t = Tracker::new(list_only);
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(37) {
            let s: String = chunk.iter().collect();
            for ev in p.feed(&s) {
                t.apply(ev);
            }
        }
        for ev in p.finish() {
            t.apply(ev);
        }
        t.finish();
        (t, code)
    }

    #[test]
    fn success() {
        let (t, code) = track("success", false);
        assert_eq!(code, 1);
        assert_eq!((t.files_done, t.bytes_done), (4, 41_943_050));
        assert!(t.failed.is_empty() && t.retries.is_empty() && t.errors.is_empty());
        let s = t.summary.unwrap_or_default();
        assert_eq!((s.files.total, s.files.copied), (4, 4));
        assert_eq!(s.bytes.copied, 41_943_050);
        assert_eq!(s.dirs.copied, 3);
        assert!(!exit_meaning(code).failed);
    }

    #[test]
    fn nothing_to_copy() {
        let (t, code) = track("nothing", false);
        assert_eq!(code, 0);
        assert_eq!(t.files_done, 0);
        let s = t.summary.unwrap_or_default();
        assert_eq!((s.files.total, s.files.skipped), (4, 4));
        assert_eq!(exit_meaning(0).word, "nothing to copy");
    }

    #[test]
    fn locked_file_retries_then_fails() {
        let (t, code) = track("locked", false);
        assert_eq!(code, 9);
        assert_eq!(t.files_done, 3);
        assert_eq!(t.failed.len(), 1);
        let f = &t.failed[0];
        assert!(f.path.ends_with(r"sub\b.txt"), "{f:?}");
        assert_eq!(f.code, 32);
        assert!(f.message.contains("being used by another process"));
        assert_eq!(t.retries.len(), 1);
        assert_eq!((t.retries[0].attempts, t.retries[0].ok), (2, Some(false)));
        assert_eq!(t.summary.map(|s| s.files.failed), Some(1));
        let m = exit_meaning(code);
        assert!(m.failed);
        assert_eq!(m.parts, "1 (files copied) + 8 (some copies failed)");
    }

    #[test]
    fn access_denied() {
        let (t, code) = track("denied", false);
        assert_eq!(code, 9);
        assert_eq!(t.failed.len(), 1);
        assert_eq!(t.failed[0].code, 5);
        assert!(t.failed[0].path.ends_with(r"src\a.txt"));
        assert_eq!(t.failed[0].message, "Access is denied.");
        assert_eq!(t.files_done, 3);
    }

    #[test]
    fn mirror_extras() {
        let (t, code) = track("mirror_extra", false);
        assert_eq!(code, 3);
        assert_eq!(t.extras.len(), 2);
        assert!(t.extras.iter().any(|(_, e)| e.ends_with(r"old\stale.txt")));
        assert_eq!(t.files_done, 4);
        assert_eq!(t.summary.map(|s| s.files.extras), Some(2));
    }

    #[test]
    fn list_only() {
        let (t, code) = track("list", true);
        assert_eq!(code, 3);
        assert_eq!(t.files_done, 0, "/L copies nothing");
        let classes: Vec<&FileClass> = t.listed.iter().map(|(c, _, _)| c).collect();
        assert_eq!(
            classes,
            [
                &FileClass::Older,
                &FileClass::New,
                &FileClass::New,
                &FileClass::New
            ]
        );
        assert_eq!(t.extras.len(), 1);
        assert_eq!(t.summary.map(|s| s.bytes.copied), Some(41_943_050));
    }

    #[test]
    fn multithreaded() {
        let (t, code) = track("mt", false);
        assert_eq!(code, 1);
        assert_eq!((t.files_done, t.bytes_done), (4, 41_943_050));
    }

    #[test]
    fn missing_source_and_unreachable_dest() {
        let (t, code) = track("missing_source", false);
        assert_eq!(code, 16);
        assert_eq!(t.errors.len(), 1);
        assert_eq!(t.errors[0].code, 3);
        assert_eq!(t.errors[0].action, "Accessing Source Directory");
        assert_eq!(exit_meaning(code).word, "fatal");

        let (t, code) = track("unreachable", false);
        assert_eq!(code, 16);
        // File system type, Accessing and Creating the destination: once each despite
        // the retries.
        assert_eq!(t.errors.len(), 3, "{:#?}", t.errors);
        assert!(t.errors.iter().all(|e| e.code == 53
            && e.path.as_deref() == Some(r"\\10.255.255.1\x\dest\")
            && e.message == "The network path was not found."));
    }

    #[test]
    fn progress_counts_the_current_file() {
        let mut p = Parser::new();
        let mut t = Tracker::new(false);
        let evs = p.feed("\t    New File  \t\t     1000\tC:\\x\\big.bin\r 25.0%\r 50.0%\r");
        for e in evs {
            t.apply(e);
        }
        assert_eq!(t.current_path(), Some(r"C:\x\big.bin"));
        assert_eq!(t.bytes_now(), 500);
        for e in p.feed("100%  \r\n") {
            t.apply(e);
        }
        assert_eq!(
            (t.files_done, t.bytes_now(), t.current_path()),
            (1, 1000, None)
        );
    }

    #[test]
    fn retry_that_succeeds() {
        let text = "\t    New File  \t\t       6\tC:\\s\\a.txt\r\n\
2026/11/06 10:50:35 ERROR 32 (0x00000020) Copying File C:\\s\\a.txt\r\n\
The process cannot access the file because it is being used by another process.\r\r\n\
Waiting 5 seconds... Retrying...\r\n\
\t    New File  \t\t       6\tC:\\s\\a.txt\r100%  \r\n";
        let mut p = Parser::new();
        let mut t = Tracker::new(false);
        for e in p.feed(text) {
            t.apply(e);
        }
        t.finish();
        assert!(t.failed.is_empty());
        assert_eq!(t.retries.len(), 1);
        assert_eq!((t.retries[0].attempts, t.retries[0].ok), (2, Some(true)));
        assert_eq!(t.files_done, 1);
    }

    #[test]
    fn translated_labels_still_parse() {
        // A French robocopy: classes and the error word are translated.
        let text = "\t    Nouveau fichier  \t\t      12\tD:\\p\\é.txt\r100%  \r\n\
2026/11/06 10:50:35 ERREUR 5 (0x00000005) Copie du fichier D:\\p\\b.txt\r\n\
Accès refusé.\r\n";
        let mut p = Parser::new();
        let evs = p.feed(text);
        assert!(matches!(
            &evs[0],
            Event::File {
                class: FileClass::Other(_),
                size: 12,
                ..
            }
        ));
        assert!(matches!(&evs[1], Event::Progress(x) if *x == 100.0));
        let Event::Error(e) = &evs[2] else {
            panic!("{evs:?}")
        };
        assert_eq!((e.code, e.path.as_deref()), (5, Some(r"D:\p\b.txt")));
        assert_eq!(e.message, "Accès refusé.");
    }

    #[test]
    fn sizes_without_bytes_flag() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1.5 m"), Some(1_572_864));
        assert_eq!(parse_size("2 g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn exit_codes() {
        let table = [
            (0, false, "nothing to copy"),
            (1, false, "copied"),
            (2, false, "done"),
            (3, false, "copied"),
            (8, true, "failed"),
            (9, true, "failed"),
            (16, true, "fatal"),
            (-1, true, "failed"),
        ];
        for (code, failed, word) in table {
            let m = exit_meaning(code);
            assert_eq!((m.failed, m.word), (failed, word), "{code}");
        }
        assert_eq!(
            exit_meaning(3).parts,
            "1 (files copied) + 2 (extra files in the destination)"
        );
    }

    #[test]
    fn argument_table() {
        let dir = Step {
            src: r"\\srv01\p\HIT".into(),
            dst: r"D:\HIT".into(),
            files: vec![],
        };
        let files = Step {
            src: r"C:\a".into(),
            dst: r"D:\b".into(),
            files: vec!["x.txt".into(), "y z.pdf".into()],
        };
        let o = Options {
            moving: false,
            overwrite: Overwrite::Never,
            mirror: false,
            list_only: false,
            threads: 0,
            log: Some(r"C:\l\j.log".into()),
            exclude_dirs: vec![],
            exclude_files: vec![],
        };
        let a = args(&dir, &o);
        assert_eq!(&a[..3], [r"\\srv01\p\HIT", r"D:\HIT", "/E"]);
        assert!(a.contains(&"/XO".to_string()) && a.last() == Some(&r"/UNILOG+:C:\l\j.log".into()));
        let moving = Options {
            moving: true,
            overwrite: Overwrite::Always,
            threads: 8,
            ..o.clone()
        };
        let a = args(&files, &moving);
        assert_eq!(&a[..4], [r"C:\a", r"D:\b", "x.txt", "y z.pdf"]);
        assert!(!a.contains(&"/E".to_string()), "file steps don't recurse");
        assert!(a.contains(&"/MOV".to_string()) && a.contains(&"/IS".to_string()));
        assert!(a.contains(&"/MT:8".to_string()));
        assert!(args(&dir, &moving).contains(&"/MOVE".to_string()));
        let mirror = Options {
            mirror: true,
            list_only: true,
            overwrite: Overwrite::Changed,
            exclude_dirs: vec!["tmp".into(), "old dir".into()],
            exclude_files: vec!["*.bak".into()],
            ..o.clone()
        };
        let a = args(&dir, &mirror);
        assert!(a.contains(&"/MIR".to_string()) && a.contains(&"/L".to_string()));
        assert!(!a.contains(&"/XO".to_string()) && !a.contains(&"/IS".to_string()));
        let xd = a.iter().position(|s| s == "/XD").unwrap_or(0);
        assert_eq!(a[xd..xd + 5], ["/XD", "tmp", "old dir", "/XF", "*.bak"]);
        assert!(
            a.contains(&"/R:0".to_string()),
            "a dry run never waits on retries"
        );
        assert!(!args(&dir, &o).contains(&"/R:0".to_string()));
    }
}
