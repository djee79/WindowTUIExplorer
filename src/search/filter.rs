//! `f`: filtering the current folder with the search's language (CLAUDE.md section 12),
//! in place:
//!
//! ```text
//! spec            names containing "spec" (plain text: a substring, as always)
//! *.pdf  B-10??*  glob over the name          re:^B-\d{4}   regex
//! ext:pdf,dwg  type:f  size:>10mb  mtime:<7d  hidden:yes  !old
//! ```
//!
//! Filters combine with the text (`ext:pdf rev3`). `depth:` and `follow:` mean nothing
//! for one folder and are ignored. Pure.

use super::name::{Candidate, NameMatcher, Scratch};
use super::query::{Pattern, Query};
use crate::fs::{Entry, EntryKind};
use std::time::SystemTime;

pub struct DirFilter {
    /// Plain text: a case-insensitive substring of the name (lowercase).
    text: Option<String>,
    /// Everything else: glob / regex and the filters.
    matcher: NameMatcher,
    /// `hidden:yes`: show hidden / system items while filtering.
    pub hidden: bool,
}

impl DirFilter {
    /// Empty text: None (no filter). A query that doesn't parse (half typed, a bad
    /// regex): the reason, and the text is used as plain text meanwhile.
    pub fn new(text: &str) -> (Option<DirFilter>, Option<String>) {
        if text.trim().is_empty() {
            return (None, None);
        }
        let mut q = match Query::parse(text) {
            Ok(q) => q,
            Err(e) => return (plain_filter(text), Some(e.to_string())),
        };
        let words = match std::mem::replace(&mut q.pattern, Pattern::All) {
            Pattern::Fuzzy(t) => Some(t.to_lowercase()),
            other => {
                q.pattern = other;
                None
            }
        };
        let hidden = q.hidden;
        match NameMatcher::new(&q) {
            Ok(matcher) => (
                Some(DirFilter {
                    text: words,
                    matcher,
                    hidden,
                }),
                None,
            ),
            Err(e) => (plain_filter(text), Some(e)),
        }
    }

    /// Whether `e` stays in the view.
    pub fn keeps(&self, e: &Entry, now: SystemTime, scratch: &mut Scratch) -> bool {
        if let Some(t) = &self.text
            && !e.name.to_lowercase().contains(t.as_str())
        {
            return false;
        }
        let c = Candidate {
            rel: &e.name,
            name: &e.name,
            is_dir: e.kind != EntryKind::File,
            size: e.size,
            modified: e.modified,
        };
        self.matcher.score(&c, now, scratch).is_some()
    }
}

/// A query that keeps everything (only the plain text filters).
fn everything() -> Query {
    Query {
        pattern: Pattern::All,
        ext: Vec::new(),
        kind: None,
        size: None,
        age: None,
        depth: None,
        hidden: false,
        follow: false,
        exclude: Vec::new(),
    }
}

fn plain_filter(text: &str) -> Option<DirFilter> {
    Some(DirFilter {
        text: Some(text.to_lowercase()),
        matcher: NameMatcher::new(&everything()).ok()?,
        hidden: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry(name: &str, kind: EntryKind, size: u64, days_old: u64) -> Entry {
        let mut e = Entry::virtual_entry(name.to_string(), kind, String::new());
        e.size = size;
        e.modified = SystemTime::now().checked_sub(Duration::from_secs(days_old * 86_400));
        e
    }

    fn kept(filter: &str) -> Vec<String> {
        let entries = [
            entry("B-1001_rev3.pdf", EntryKind::File, 2_000_000, 1),
            entry("B-1002_rev1.PDF", EntryKind::File, 20_000_000, 30),
            entry("report.pdf.bak", EntryKind::File, 10, 1),
            entry("plan.dwg", EntryKind::File, 50_000_000, 2),
            entry("old", EntryKind::Dir, 0, 100),
            entry("pdfs", EntryKind::Dir, 0, 1),
        ];
        let (Some(f), problem) = DirFilter::new(filter) else {
            return entries.iter().map(|e| e.name.clone()).collect();
        };
        assert_eq!(problem, None, "{filter}");
        let mut s = Scratch::default();
        entries
            .iter()
            .filter(|e| f.keeps(e, SystemTime::now(), &mut s))
            .map(|e| e.name.clone())
            .collect()
    }

    #[test]
    fn text_globs_and_filters() {
        // Plain text: a substring, as before (catches .pdf.bak and the pdfs folder).
        assert_eq!(
            kept("pdf"),
            [
                "B-1001_rev3.pdf",
                "B-1002_rev1.PDF",
                "report.pdf.bak",
                "pdfs"
            ]
        );
        // The extension, exactly, any case.
        assert_eq!(kept("ext:pdf"), ["B-1001_rev3.pdf", "B-1002_rev1.PDF"]);
        assert_eq!(kept("*.pdf"), ["B-1001_rev3.pdf", "B-1002_rev1.PDF"]);
        assert_eq!(
            kept("ext:pdf,dwg"),
            ["B-1001_rev3.pdf", "B-1002_rev1.PDF", "plan.dwg"]
        );
        // Combined with text, sizes, ages, kinds, exclusions, regex.
        assert_eq!(kept("ext:pdf rev3"), ["B-1001_rev3.pdf"]);
        assert_eq!(kept("size:>10mb"), ["B-1002_rev1.PDF", "plan.dwg"]);
        assert_eq!(kept("mtime:<7d type:f ext:pdf"), ["B-1001_rev3.pdf"]);
        assert_eq!(kept("type:d"), ["old", "pdfs"]);
        assert_eq!(kept("type:d !old"), ["pdfs"]);
        assert_eq!(kept(r"re:^B-\d{4}_rev1"), ["B-1002_rev1.PDF"]);
        assert_eq!(kept("B-10??_*"), ["B-1001_rev3.pdf", "B-1002_rev1.PDF"]);
    }

    #[test]
    fn half_typed_queries_fall_back_to_plain_text() {
        let (f, problem) = DirFilter::new("size:>");
        assert!(f.is_some());
        assert!(problem.is_some_and(|p| p.contains("not a size")));
        let (_, problem) = DirFilter::new("re:([");
        assert!(problem.is_some_and(|p| p.contains("bad regex")));
        assert!(DirFilter::new("  ").0.is_none());
        assert!(DirFilter::new("hidden:yes").0.is_some_and(|f| f.hidden));
    }
}
