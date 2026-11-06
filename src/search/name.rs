//! Name matching for `s` (CLAUDE.md section 12): runs inside the walker threads.
//!
//! - **fuzzy** (nucleo, smart-case) scores the path relative to the search root, plus
//!   the file name's own score when every atom also matches there, so hits in the name
//!   rank higher (fzf / Telescope `find_files`).
//! - **glob** (globset, case-insensitive) matches the file name, or the relative path
//!   when the glob contains a separator.
//! - **regex** (smart-case) matches the file name, or the path when it mentions `\\`/`/`.
//!
//! Plus the structural filters of a [`Query`].

use super::query::{Kind, Pattern, Query};
use globset::{GlobBuilder, GlobMatcher};
use nucleo::pattern::{CaseMatching, Normalization, Pattern as Fuzzy};
use nucleo::{Config, Matcher, Utf32Str};
use regex::{Regex, RegexBuilder};
use std::time::SystemTime;

/// One walked entry, as the matcher sees it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// Path relative to the search root, `\`-separated.
    pub rel: &'a str,
    pub name: &'a str,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

enum How {
    All,
    Fuzzy(Fuzzy),
    Glob { glob: GlobMatcher, on_path: bool },
    Regex { re: Regex, on_path: bool },
}

pub struct NameMatcher {
    how: How,
    query: Query,
}

/// Per-thread matcher state (nucleo's matcher holds scratch buffers).
pub struct Scratch {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buf: Vec::new(),
        }
    }
}

/// Smart-case for regexes: an uppercase letter that isn't part of an escape (`\D`, `\S`).
fn regex_has_upper(pattern: &str) -> bool {
    let mut escaped = false;
    for c in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

impl NameMatcher {
    pub fn new(query: &Query) -> Result<NameMatcher, String> {
        let how = match &query.pattern {
            Pattern::All => How::All,
            Pattern::Fuzzy(t) => {
                How::Fuzzy(Fuzzy::parse(t, CaseMatching::Smart, Normalization::Smart))
            }
            Pattern::Glob(g) => {
                let on_path = g.contains(['\\', '/']);
                let glob = GlobBuilder::new(&g.replace('\\', "/"))
                    .case_insensitive(true)
                    .literal_separator(on_path)
                    .backslash_escape(false)
                    .build()
                    .map_err(|e| format!("bad glob: {e}"))?
                    .compile_matcher();
                How::Glob { glob, on_path }
            }
            Pattern::Regex(r) => {
                let re = RegexBuilder::new(r)
                    .case_insensitive(!regex_has_upper(r))
                    .build()
                    .map_err(|e| format!("bad regex: {e}"))?;
                How::Regex {
                    re,
                    on_path: r.contains("\\\\") || r.contains('/'),
                }
            }
        };
        Ok(NameMatcher {
            how,
            query: query.clone(),
        })
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The match score (higher is better), or None. Filters first: they're cheaper.
    pub fn score(&self, c: &Candidate, now: SystemTime, s: &mut Scratch) -> Option<u32> {
        if !self.passes_filters(c, now) {
            return None;
        }
        match &self.how {
            How::All => Some(0),
            How::Fuzzy(p) => {
                let path = p.score(Utf32Str::new(c.rel, &mut s.buf), &mut s.matcher)?;
                let name = p
                    .score(Utf32Str::new(c.name, &mut s.buf), &mut s.matcher)
                    .unwrap_or(0);
                Some(path + name)
            }
            How::Glob { glob, on_path } => {
                let hit = if *on_path {
                    glob.is_match(c.rel.replace('\\', "/"))
                } else {
                    glob.is_match(c.name)
                };
                hit.then_some(0)
            }
            How::Regex { re, on_path } => {
                let hay = if *on_path { c.rel } else { c.name };
                re.is_match(hay).then_some(0)
            }
        }
    }

    /// Whether the fuzzy score means anything (glob/regex/all only say yes or no).
    pub fn is_scored(&self) -> bool {
        matches!(self.how, How::Fuzzy(_))
    }

    fn passes_filters(&self, c: &Candidate, now: SystemTime) -> bool {
        let q = &self.query;
        match q.kind {
            Some(Kind::Dir) if !c.is_dir => return false,
            Some(Kind::File) if c.is_dir => return false,
            _ => {}
        }
        if !q.ext.is_empty() {
            let ext = c
                .name
                .rsplit_once('.')
                .filter(|(stem, _)| !stem.is_empty())
                .map(|(_, e)| e.to_lowercase());
            if c.is_dir || !ext.is_some_and(|e| q.ext.contains(&e)) {
                return false;
            }
        }
        if let Some((cmp, bound)) = q.size
            && (c.is_dir || !cmp.test(c.size, bound))
        {
            return false;
        }
        if let Some((cmp, bound)) = q.age {
            let Some(age) = c.modified.and_then(|m| now.duration_since(m).ok()) else {
                return false;
            };
            if !cmp.test(age, bound) {
                return false;
            }
        }
        !self.excluded(c.rel)
    }

    /// `!old`: any path segment equal to an excluded one (case-insensitive).
    pub fn excluded(&self, rel: &str) -> bool {
        !self.query.exclude.is_empty()
            && rel.split('\\').any(|seg| {
                self.query
                    .exclude
                    .iter()
                    .any(|x| seg.eq_ignore_ascii_case(x))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn m(q: &str) -> NameMatcher {
        let Ok(query) = Query::parse(q) else {
            panic!("{q}")
        };
        match NameMatcher::new(&query) {
            Ok(m) => m,
            Err(e) => panic!("{q}: {e}"),
        }
    }

    fn file(rel: &str) -> Candidate<'_> {
        Candidate {
            rel,
            name: rel.rsplit('\\').next().unwrap_or(rel),
            is_dir: false,
            size: 1024,
            modified: Some(SystemTime::now() - Duration::from_secs(3600)),
        }
    }

    fn hits<'a>(q: &str, paths: &[&'a str]) -> Vec<&'a str> {
        let (matcher, mut s, now) = (m(q), Scratch::default(), SystemTime::now());
        let mut scored: Vec<(u32, &str)> = paths
            .iter()
            .filter_map(|p| matcher.score(&file(p), now, &mut s).map(|sc| (sc, *p)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        scored.into_iter().map(|(_, p)| p).collect()
    }

    const TREE: &[&str] = &[
        r"hitiso\B-1001_rev3.pdf",
        r"hitiso\old\B-1001_rev2.pdf",
        r"hitrep\iso_B-1001_bom.csv",
        r"hitmac\iso\B1001_fix.pmlmac",
        r"hitdfl\readme.txt",
        r"B1001\notes.txt",
    ];

    #[test]
    fn fuzzy_finds_fragments_in_any_order() {
        let got = hits("b1001 iso", TREE);
        assert!(got.contains(&r"hitiso\B-1001_rev3.pdf"), "{got:?}");
        assert!(got.contains(&r"hitmac\iso\B1001_fix.pmlmac"), "{got:?}");
        assert!(!got.contains(&r"hitdfl\readme.txt"));
        assert_eq!(hits("iso b1001", TREE).len(), got.len(), "order of atoms");
    }

    #[test]
    fn name_hits_rank_higher() {
        // Both paths contain "notes"; only one has it in the file name.
        let got = hits("notes", &[r"notes\a.txt", r"x\notes.txt"]);
        assert_eq!(got.first(), Some(&r"x\notes.txt"), "{got:?}");
    }

    #[test]
    fn smart_case() {
        assert_eq!(hits("README", TREE), Vec::<&str>::new());
        assert_eq!(hits("readme", TREE), [r"hitdfl\readme.txt"]);
    }

    #[test]
    fn globs_and_regex() {
        let pdfs = hits("*.pdf", TREE);
        assert_eq!(pdfs.len(), 2, "{pdfs:?}");
        assert_eq!(hits("*.PDF", TREE).len(), 2, "globs are case-insensitive");
        assert_eq!(hits("B-10??_rev*", TREE).len(), 2);
        assert_eq!(
            hits(r"hitiso\*.pdf", TREE),
            [r"hitiso\B-1001_rev3.pdf"],
            "path glob"
        );
        assert_eq!(hits(r"re:^B-\d{4}_rev\d", TREE).len(), 2);
        assert_eq!(hits(r"re:_REV", TREE).len(), 0, "regex smart-case");
        assert!(NameMatcher::new(&Query::parse("re:(").unwrap_or_else(|_| panic!())).is_err());
    }

    #[test]
    fn filters() {
        assert_eq!(hits("ext:csv,pmlmac", TREE).len(), 2);
        assert_eq!(hits("iso !old ext:pdf", TREE), [r"hitiso\B-1001_rev3.pdf"]);
        assert_eq!(hits("size:>2kb", TREE).len(), 0);
        assert_eq!(hits("size:<2kb", TREE).len(), TREE.len());
        assert_eq!(hits("mtime:<1d", TREE).len(), TREE.len());
        assert_eq!(hits("mtime:>1d", TREE).len(), 0);
        let (mm, mut s) = (m("type:d"), Scratch::default());
        assert!(
            mm.score(&file("x.txt"), SystemTime::now(), &mut s)
                .is_none()
        );
        let dir = Candidate {
            is_dir: true,
            ..file("hitiso")
        };
        assert!(mm.score(&dir, SystemTime::now(), &mut s).is_some());
    }
}
