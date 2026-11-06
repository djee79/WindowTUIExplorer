//! Recursive search whose results become a folder (CLAUDE.md section 12).
//!
//! - [`query`]: the query language (fuzzy / glob / `re:` + filters). Pure.
//! - [`name`]: matching and scoring, run inside the walker threads.
//! - [`walk`]: the parallel walk that streams hits to the UI.
//!
//! The results folder itself (state, sorting, history) lives in `app::search`.

pub mod name;
pub mod query;
pub mod walk;
