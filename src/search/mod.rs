//! Recursive search whose results become a folder (CLAUDE.md section 12).
//!
//! - [`query`]: the query language (fuzzy / glob / `re:` + filters). Pure.
//! - [`name`]: name matching and scoring (`s`), run inside the walker threads.
//! - [`content`]: content matching (`S`, ripgrep's crates), also in the walker threads.
//! - [`walk`]: the parallel walk that streams hits to the UI.
//!
//! The results folder itself (state, sorting, history) lives in `app::search`.

pub mod content;
pub mod name;
pub mod query;
pub mod walk;

use content::ContentMatcher;
use name::NameMatcher;
use query::Query;

/// What a walk matches: names (`s`) or contents (`S`).
pub enum SearchMatcher {
    Name(NameMatcher),
    Content(ContentMatcher),
}

impl SearchMatcher {
    /// `content` picks `S` semantics for the same query text.
    pub fn new(query: &Query, content: bool) -> Result<SearchMatcher, String> {
        Ok(if content {
            SearchMatcher::Content(ContentMatcher::new(query)?)
        } else {
            SearchMatcher::Name(NameMatcher::new(query)?)
        })
    }

    pub fn query(&self) -> &Query {
        match self {
            SearchMatcher::Name(m) => m.query(),
            SearchMatcher::Content(m) => m.query(),
        }
    }

    /// Whether a hit's score means something: a fuzzy score or a match count.
    pub fn is_scored(&self) -> bool {
        match self {
            SearchMatcher::Name(m) => m.is_scored(),
            SearchMatcher::Content(_) => true,
        }
    }
}
