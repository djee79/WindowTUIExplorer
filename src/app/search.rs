//! Search results as a folder (CLAUDE.md section 12): one [`SearchState`] per search,
//! shown at `Loc::Results { id, origin }`, part of the directory history.

use super::App;
use crate::fs::path::Loc;
use crate::fs::worker::CancelToken;
use crate::fs::{Entry, EntryKind, Listing, natural_cmp};
use crate::search::name::NameMatcher;
use crate::search::query::Query;
use crate::search::walk::Hit;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Searches kept for `H` / `L`; older ones are cancelled and dropped.
const MAX_SEARCHES: usize = 8;
/// Re-sort the results at most this often while hits stream in.
const REBUILD_EVERY: Duration = Duration::from_millis(50);
/// Results shown are capped so a `*` search on a huge share stays usable.
pub const MAX_HITS: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Score,
    Path,
    Size,
    Modified,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Score => "score",
            SortMode::Path => "path",
            SortMode::Size => "size",
            SortMode::Modified => "modified",
        }
    }
    fn next(self) -> SortMode {
        match self {
            SortMode::Score => SortMode::Path,
            SortMode::Path => SortMode::Size,
            SortMode::Size => SortMode::Modified,
            SortMode::Modified => SortMode::Score,
        }
    }
}

pub struct SearchState {
    pub id: u64,
    pub query: String,
    pub origin: Loc,
    pub hits: Vec<Hit>,
    seen: HashSet<String>,
    pub scanned: u64,
    pub errors: u64,
    pub started: Instant,
    /// Set when the walk ends.
    pub elapsed: Option<Duration>,
    pub cancelled: bool,
    pub current: String,
    pub last_progress: Instant,
    pub sort: SortMode,
    /// Fuzzy scores mean something; glob / regex / filter-only results have none.
    pub scored: bool,
    pub truncated: bool,
    cancel: CancelToken,
    listing: Option<Arc<Listing>>,
    dirty: bool,
    last_build: Instant,
}

impl SearchState {
    pub fn running(&self) -> bool {
        self.elapsed.is_none()
    }

    pub fn took(&self) -> Duration {
        self.elapsed.unwrap_or_else(|| self.started.elapsed())
    }

    fn add(&mut self, hits: Vec<Hit>) {
        for h in hits {
            if self.hits.len() >= MAX_HITS {
                self.truncated = true;
                break;
            }
            if self.seen.insert(h.rel.to_lowercase()) {
                self.hits.push(h);
                self.dirty = true;
            }
        }
    }

    fn build(&mut self) {
        let mut order: Vec<&Hit> = self.hits.iter().collect();
        match self.sort {
            SortMode::Score => order.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| natural_cmp(&a.rel, &b.rel))
            }),
            SortMode::Path => order.sort_by(|a, b| natural_cmp(&a.rel, &b.rel)),
            SortMode::Size => order.sort_by_key(|h| std::cmp::Reverse(h.size)),
            SortMode::Modified => order.sort_by_key(|h| std::cmp::Reverse(h.modified)),
        }
        let entries = order
            .into_iter()
            .map(|h| Entry {
                name: h.rel.clone(),
                kind: if h.is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                is_link: h.is_link,
                size: h.size,
                modified: h.modified,
                created: h.created,
                attrs: h.attrs,
                detail: if self.scored {
                    h.score.to_string()
                } else {
                    String::new()
                },
            })
            .collect();
        self.listing = Some(Arc::new(Listing {
            entries,
            loaded_at: Instant::now(),
        }));
        self.dirty = false;
        self.last_build = Instant::now();
    }
}

impl App {
    pub fn search_state(&self, loc: &Loc) -> Option<&SearchState> {
        let Loc::Results { id, .. } = loc else {
            return None;
        };
        self.searches.iter().find(|s| s.id == *id)
    }

    fn search_mut(&mut self, id: u64) -> Option<&mut SearchState> {
        self.searches.iter_mut().find(|s| s.id == id)
    }

    /// The listing a results location shows.
    pub(super) fn results_listing(&self, loc: &Loc) -> Option<Arc<Listing>> {
        self.search_state(loc).and_then(|s| s.listing.clone())
    }

    /// The query of the results folder we're in, to prefill `s` for refining.
    pub(super) fn current_search_query(&self) -> Option<String> {
        self.search_state(&self.cwd).map(|s| s.query.clone())
    }

    /// `s` + Enter: start a walk from the real folder we're in. From inside results,
    /// the hits found so far are re-filtered at once while the fresh walk runs.
    pub(super) fn start_search(&mut self, text: &str) {
        let query = match Query::parse(text) {
            Ok(q) => q,
            Err(e) => return self.set_status(format!("search: {e}"), true),
        };
        let matcher = match NameMatcher::new(&query) {
            Ok(m) => Arc::new(m),
            Err(e) => return self.set_status(format!("search: {e}"), true),
        };
        let origin = self.cwd.real().clone();
        let Some(root) = origin.as_fs().cloned() else {
            return self.set_status(
                "search runs inside a folder (not Locations or a server)".into(),
                true,
            );
        };
        let id = self.next_search_id;
        self.next_search_id += 1;
        let previous = self.search_state(&self.cwd).map(|s| s.hits.clone());
        let cancel = self.worker.search(id, &root, Arc::clone(&matcher));
        if let Some(hits) = previous.filter(|h| !h.is_empty()) {
            self.worker.rescore(id, hits, Arc::clone(&matcher));
        }
        let now = Instant::now();
        self.searches.push(SearchState {
            id,
            query: text.trim().to_string(),
            origin: origin.clone(),
            hits: Vec::new(),
            seen: HashSet::new(),
            scanned: 0,
            errors: 0,
            started: now,
            elapsed: None,
            cancelled: false,
            current: String::new(),
            last_progress: now,
            sort: SortMode::Score,
            scored: matcher.is_scored(),
            truncated: false,
            cancel,
            listing: None,
            dirty: true,
            last_build: now - REBUILD_EVERY,
        });
        if self.searches.len() > MAX_SEARCHES {
            let old = self.searches.remove(0);
            old.cancel.cancel();
        }
        if let Some(s) = self.search_mut(id) {
            s.build();
        }
        // Refining replaces the results we're in rather than stacking another level.
        let target = Loc::Results {
            id,
            origin: Box::new(origin),
        };
        if matches!(self.cwd, Loc::Results { .. }) {
            self.set_cwd(target, None);
        } else {
            self.navigate(target, None);
        }
    }

    pub(super) fn on_search_progress(
        &mut self,
        id: u64,
        hits: Vec<Hit>,
        scanned: u64,
        current: String,
    ) {
        let Some(s) = self.search_mut(id) else { return };
        s.scanned = s.scanned.max(scanned);
        s.last_progress = Instant::now();
        if !current.is_empty() {
            s.current = current;
        }
        s.add(hits);
        self.dirty = true;
    }

    pub(super) fn on_search_done(
        &mut self,
        id: u64,
        scanned: u64,
        errors: u64,
        elapsed: Duration,
        cancelled: bool,
    ) {
        let Some(s) = self.search_mut(id) else { return };
        s.scanned = s.scanned.max(scanned);
        s.errors = errors;
        s.elapsed = Some(elapsed);
        s.cancelled = cancelled;
        s.build();
        let msg = format!(
            "search {}: {} found in {:.1} s{}{}",
            if cancelled { "cancelled" } else { "done" },
            crate::ui::thousands(s.hits.len() as u64),
            elapsed.as_secs_f32(),
            if errors > 0 {
                format!(" Â· {errors} unreadable")
            } else {
                String::new()
            },
            if s.truncated { " Â· capped" } else { "" },
        );
        let here = self.search_state(&self.cwd).is_some_and(|s| s.id == id);
        if here {
            self.rebuild_view();
            self.set_status(msg, false);
        }
    }

    /// From `tick`: re-sort the visible results while hits stream in.
    pub(super) fn search_tick(&mut self, now: Instant) {
        let Loc::Results { id, .. } = self.cwd else {
            return;
        };
        let Some(s) = self.search_mut(id) else { return };
        if s.dirty && now.duration_since(s.last_build) >= REBUILD_EVERY {
            s.build();
            self.rebuild_view();
        }
        if self
            .search_state(&self.cwd)
            .is_some_and(SearchState::running)
        {
            self.dirty = true; // elapsed timer + spinner
        }
    }

    pub(super) fn searching(&self) -> bool {
        self.searches.iter().any(SearchState::running)
    }

    /// `Esc` in results: stop a running walk (results stay), else go back where the
    /// search started. Returns false when not in results.
    pub(super) fn results_escape(&mut self) -> bool {
        let Loc::Results { id, origin } = self.cwd.clone() else {
            return false;
        };
        if let Some(s) = self.search_mut(id).filter(|s| s.running()) {
            s.cancel.cancel();
            self.set_status("cancelling the walk; the results stay".into(), false);
            return true;
        }
        if self.back.last() == Some(&*origin) {
            self.history(1, true);
        } else {
            self.navigate(*origin, None);
        }
        true
    }

    /// `gr`: the real folder of the result under the cursor, with it selected.
    pub(super) fn goto_result_folder(&mut self) {
        if !matches!(self.cwd, Loc::Results { .. }) {
            return self.set_status("gr works in search results".into(), false);
        }
        let Some(name) = self.selected().map(|e| e.name.clone()) else {
            return;
        };
        match self.cwd.join(&name) {
            Ok(target) => {
                let select = target.name();
                if let Some(dir) = target.parent() {
                    self.navigate(dir, Some(select));
                }
            }
            Err(e) => self.set_status(e.to_string(), true),
        }
    }

    /// `,s`: score â†’ path â†’ size â†’ modified.
    pub(super) fn cycle_sort(&mut self) {
        let Loc::Results { id, .. } = self.cwd else {
            return self.set_status(",s sorts search results".into(), false);
        };
        let Some(s) = self.search_mut(id) else { return };
        s.sort = s.sort.next();
        let label = s.sort.label();
        s.build();
        self.rebuild_view();
        self.set_status(format!("sort: {label}"), false);
    }
}
