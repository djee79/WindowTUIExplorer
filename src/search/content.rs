//! Content matching for `S` (CLAUDE.md section 12), on ripgrep's crates.
//!
//! - The query's text is a **literal** (smart-case) unless it starts with `re:`. Name
//!   filters (`ext:`, `size:`, `mtime:`, `!seg`, `depth:`, `hidden:`) still apply, so
//!   `S foo ext:pmlmac,pmlfrm` only opens PML files.
//! - Binary files (a NUL byte) are skipped, as ripgrep does. UTF-16 files with a BOM are
//!   transcoded first, so `.bat`/`.ps1` files saved by Notepad as "Unicode" are searched.
//! - [`ContentMatcher::count`] runs inside the walker threads and returns the number of
//!   matching lines. [`ContentMatcher::preview`] runs on the blocking pool for the file
//!   under the cursor and returns the matches with context lines and highlight ranges.

use super::name::{Candidate, NameMatcher, Scratch};
use super::query::{Kind, Pattern, Query};
use crate::fs::worker::CancelToken;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use std::io;
use std::time::SystemTime;

pub struct ContentMatcher {
    /// The query's filters only (its pattern is the content pattern, not a name one).
    filters: NameMatcher,
    re: RegexMatcher,
}

/// One line of a match preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewLine {
    /// 1-based.
    pub number: u64,
    /// Without the line terminator; invalid UTF-8 is replaced.
    pub text: String,
    /// Byte ranges of the matches in `text`; empty for a context line.
    pub hits: Vec<(usize, usize)>,
    /// Lines were skipped between the previous line and this one.
    pub gap_before: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchPreview {
    pub lines: Vec<PreviewLine>,
    /// Indices into `lines` of the matching lines, in file order.
    pub matches: Vec<usize>,
    /// Stopped at the match limit; the file has more.
    pub truncated: bool,
    /// The file turned out to be binary (it changed since the walk, or a NUL came late).
    pub binary: bool,
}

/// The pattern for a content search: literal text, or a regex after `re:`.
fn pattern_of(q: &Query) -> Result<(String, bool), String> {
    match &q.pattern {
        Pattern::All => Err("search contents for what? (text, or re:regex)".to_string()),
        // `*` and `?` mean nothing special in file contents: literal like the rest.
        Pattern::Fuzzy(t) | Pattern::Glob(t) => Ok((t.clone(), true)),
        Pattern::Regex(r) => Ok((r.clone(), false)),
    }
}

impl ContentMatcher {
    pub fn new(query: &Query) -> Result<ContentMatcher, String> {
        let (pattern, literal) = pattern_of(query)?;
        let re = RegexMatcherBuilder::new()
            .case_smart(true)
            .fixed_strings(literal)
            // As ripgrep does: `^`/`$` at line boundaries, and `$` before a CRLF.
            .multi_line(true)
            .crlf(true)
            .line_terminator(Some(b'\n'))
            .build(&pattern)
            .map_err(|e| format!("bad regex: {e}"))?;
        let filter_query = Query {
            pattern: Pattern::All,
            kind: Some(Kind::File),
            ..query.clone()
        };
        let filters = NameMatcher::new(&filter_query)?;
        Ok(ContentMatcher { filters, re })
    }

    /// The query's filters (the walker reads depth, hidden, follow and excludes there).
    pub fn query(&self) -> &Query {
        self.filters.query()
    }

    /// Name-level filters, before any byte is read. Directories never pass.
    pub fn passes(&self, c: &Candidate, now: SystemTime, s: &mut Scratch) -> bool {
        self.filters.score(c, now, s).is_some()
    }

    fn searcher(context: usize) -> Searcher {
        SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(true)
            .before_context(context)
            .after_context(context)
            .build()
    }

    /// The number of matching lines in the file at `path` (an OS path string), or None
    /// for no match or a binary file. Stops early on cancel.
    pub fn count(&self, path: &str, cancel: &CancelToken) -> io::Result<Option<u32>> {
        let mut sink = CountSink {
            count: 0,
            binary: false,
            cancel,
        };
        Self::searcher(0).search_path(&self.re, path, &mut sink)?;
        Ok((!sink.binary && sink.count > 0).then_some(sink.count))
    }

    /// Up to `max_matches` matching lines of `path`, each with `context` lines around it.
    pub fn preview(
        &self,
        path: &str,
        context: usize,
        max_matches: usize,
    ) -> io::Result<MatchPreview> {
        let mut sink = PreviewSink {
            re: &self.re,
            max: max_matches,
            out: MatchPreview::default(),
            last_number: None,
        };
        Self::searcher(context).search_path(&self.re, path, &mut sink)?;
        Ok(sink.out)
    }
}

/// Match ranges within one line (for highlighting), on char boundaries.
fn match_ranges(re: &RegexMatcher, text: &str) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    let _ = re.find_iter(text.as_bytes(), |m| {
        if m.start() < m.end() && text.is_char_boundary(m.start()) && text.is_char_boundary(m.end())
        {
            hits.push((m.start(), m.end()));
        }
        true
    });
    hits
}

struct CountSink<'a> {
    count: u32,
    binary: bool,
    cancel: &'a CancelToken,
}

impl Sink for CountSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.count = self.count.saturating_add(1);
        Ok(!self.cancel.is_cancelled())
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

struct PreviewSink<'a> {
    re: &'a RegexMatcher,
    max: usize,
    out: MatchPreview,
    last_number: Option<u64>,
}

/// One line's text: no terminator, lossy UTF-8.
fn line_text(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

impl PreviewSink<'_> {
    fn push(&mut self, number: u64, bytes: &[u8], is_match: bool) {
        let text = line_text(bytes);
        let hits = if is_match {
            match_ranges(self.re, &text)
        } else {
            Vec::new()
        };
        let gap_before = self.last_number.is_some_and(|n| number > n + 1);
        self.last_number = Some(number);
        if is_match {
            self.out.matches.push(self.out.lines.len());
        }
        self.out.lines.push(PreviewLine {
            number,
            text,
            hits,
            gap_before,
        });
    }
}

impl Sink for PreviewSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.out.matches.len() >= self.max {
            self.out.truncated = true;
            return Ok(false);
        }
        let first = m.line_number().unwrap_or(0);
        // A match block can span several lines (it doesn't here: no multi-line mode),
        // but handle it the general way.
        for (i, line) in m.lines().enumerate() {
            self.push(first + i as u64, line, true);
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, io::Error> {
        // After-context of the last match we keep is fine; before-context past the
        // limit isn't wanted.
        if self.out.matches.len() >= self.max && *c.kind() == SinkContextKind::Before {
            return Ok(true);
        }
        self.push(c.line_number().unwrap_or(0), c.bytes(), false);
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, io::Error> {
        self.out.binary = true;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cm(q: &str) -> ContentMatcher {
        let Ok(query) = Query::parse(q) else {
            panic!("{q}")
        };
        match ContentMatcher::new(&query) {
            Ok(m) => m,
            Err(e) => panic!("{q}: {e}"),
        }
    }

    fn file(tag: &str, bytes: &[u8]) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("sentier-content-{tag}-{}.txt", std::process::id()));
        let _ = std::fs::write(&p, bytes);
        p
    }

    fn count(q: &str, bytes: &[u8], tag: &str) -> Option<u32> {
        let p = file(tag, bytes);
        let got = cm(q).count(&p.to_string_lossy(), &CancelToken::default());
        let _ = std::fs::remove_file(&p);
        match got {
            Ok(n) => n,
            Err(e) => panic!("{q}: {e}"),
        }
    }

    const PML: &[u8] = b"-- B-1001 macro\r\nNEW PIPE /B-1001\r\n  SPEC /A1A\r\nhandle any\r\nendhandle\r\n!x = 'b-1001'\r\n";

    #[test]
    fn literal_smart_case_counts_lines() {
        assert_eq!(count("B-1001", PML, "a"), Some(2), "uppercase: exact case");
        assert_eq!(count("b-1001", PML, "b"), Some(3), "lowercase: any case");
        assert_eq!(count("handle", PML, "c"), Some(2));
        assert_eq!(count("nothing here", PML, "d"), None);
        // Literal: the dot and the star are not regex syntax.
        assert_eq!(count("a.c*", b"abc\na.c*\n", "e"), Some(1));
    }

    #[test]
    fn regex_and_filters() {
        assert_eq!(count(r"re:^\s+SPEC\b", PML, "f"), Some(1));
        assert_eq!(count(r"re:endhandle$", PML, "g"), Some(1), "$ before CRLF");
        let Ok(q) = Query::parse("re:(") else {
            panic!()
        };
        assert!(ContentMatcher::new(&q).is_err(), "bad regex");
        let Ok(q) = Query::parse("ext:pdf") else {
            panic!()
        };
        assert!(ContentMatcher::new(&q).is_err(), "no text");

        let (m, mut s) = (cm("x ext:pmlmac"), Scratch::default());
        let cand = |name: &'static str, is_dir| Candidate {
            rel: name,
            name,
            is_dir,
            size: 10,
            modified: None,
        };
        let now = SystemTime::now();
        assert!(m.passes(&cand("a.pmlmac", false), now, &mut s));
        assert!(!m.passes(&cand("a.txt", false), now, &mut s), "ext filter");
        assert!(
            !m.passes(&cand("dir.pmlmac", true), now, &mut s),
            "files only"
        );
    }

    #[test]
    fn binary_skipped_utf16_searched() {
        assert_eq!(count("PIPE", b"PIPE\x00\x01\x02 PIPE", "h"), None, "binary");
        // UTF-16 LE with BOM, as Notepad's "Unicode" saves it.
        let mut utf16 = vec![0xFF, 0xFE];
        for u in "set PROJ=HIT\r\ncall evars.bat\r\n".encode_utf16() {
            utf16.extend(u.to_le_bytes());
        }
        assert_eq!(count("evars", &utf16, "i"), Some(1), "utf-16 with BOM");
    }

    #[test]
    fn preview_has_context_gaps_and_ranges() {
        let text = (1..=20)
            .map(|n| {
                if n == 5 || n == 15 {
                    format!("x{n} PIPE PIPE")
                } else {
                    format!("line {n}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let p = file("p", text.as_bytes());
        let path = p.to_string_lossy().to_string();
        let got = cm("PIPE").preview(&path, 2, 100);
        let short = cm("PIPE").preview(&path, 0, 1);
        let _ = std::fs::remove_file(&p);
        let Ok(got) = got else { panic!() };
        let numbers: Vec<u64> = got.lines.iter().map(|l| l.number).collect();
        assert_eq!(numbers, [3, 4, 5, 6, 7, 13, 14, 15, 16, 17]);
        assert_eq!(got.matches, [2, 7]);
        assert!(got.lines[5].gap_before && !got.lines[1].gap_before);
        assert_eq!(got.lines[2].text, "x5 PIPE PIPE");
        assert_eq!(got.lines[2].hits, [(3, 7), (8, 12)]);
        assert!(got.lines[3].hits.is_empty(), "context line");
        assert!(!got.truncated);

        let Ok(short) = short else { panic!() };
        assert_eq!(short.matches.len(), 1);
        assert!(short.truncated, "match limit");
    }
}
