//! Search results as a folder (CLAUDE.md section 12): one [`SearchState`] per search,
//! shown at `Loc::Results { id, origin }`, part of the directory history.

use super::App;
use crate::fs::path::Loc;
use crate::fs::sort::{DirSort, SortKey};
use crate::fs::worker::CancelToken;
use crate::fs::{Entry, EntryKind, Listing, natural_cmp};
use crate::search::SearchMatcher;
use crate::search::content::MatchPreview;
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
    /// `,r` relevance (the default), `,n` path, `,m` `,c` dates, `,s` size, `,e` extension.
    pub sort: DirSort,
    /// `S`: the hits are files whose contents matched; the score is their match count.
    pub content: bool,
    /// Kept for the match preview of content results.
    matcher: Arc<SearchMatcher>,
    /// Fuzzy scores and match counts mean something; glob / regex / filter-only name
    /// results have none.
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

    /// Content results sort by match count where name results sort by score.
    pub fn sort_label(&self) -> String {
        match self.sort.key {
            SortKey::Relevance if self.content => "matches".to_string(),
            SortKey::Relevance => "score".to_string(),
            _ => self.sort.label(),
        }
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
        let ext = |h: &Hit| {
            let name = h.rel.rsplit('\\').next().unwrap_or(&h.rel);
            match name.rsplit_once('.') {
                Some((stem, e)) if !stem.is_empty() && !h.is_dir => e.to_lowercase(),
                _ => String::new(),
            }
        };
        match self.sort.key {
            SortKey::Relevance => order.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| natural_cmp(&a.rel, &b.rel))
            }),
            SortKey::Name => order.sort_by(|a, b| natural_cmp(&a.rel, &b.rel)),
            SortKey::Size => order.sort_by_key(|h| std::cmp::Reverse(h.size)),
            SortKey::Modified => order.sort_by_key(|h| std::cmp::Reverse(h.modified)),
            SortKey::Created => order.sort_by_key(|h| std::cmp::Reverse(h.created)),
            SortKey::Ext => order.sort_by(|a, b| {
                ext(a)
                    .cmp(&ext(b))
                    .then_with(|| natural_cmp(&a.rel, &b.rel))
            }),
        }
        if self.sort.reverse {
            order.reverse();
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

    /// The query of the results folder we're in, to prefill `s` / `S` for refining
    /// (only a search of the same kind).
    pub(super) fn current_search_query(&self, content: bool) -> Option<String> {
        self.search_state(&self.cwd)
            .filter(|s| s.content == content)
            .map(|s| s.query.clone())
    }

    /// `s` / `S` + Enter: start a walk from the real folder we're in. Refining a name
    /// search re-filters the hits found so far at once while the fresh walk runs.
    pub(super) fn start_search(&mut self, text: &str, content: bool) {
        let what = if content { "grep" } else { "search" };
        let query = match Query::parse(text) {
            Ok(q) => q,
            Err(e) => return self.set_status(format!("{what}: {e}"), true),
        };
        let matcher = match SearchMatcher::new(&query, content) {
            Ok(m) => Arc::new(m),
            Err(e) => return self.set_status(format!("{what}: {e}"), true),
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
        // Same kind of search as the results we're in: this refines them.
        let refining = self
            .search_state(&self.cwd)
            .is_some_and(|s| s.content == content);
        let previous = self
            .search_state(&self.cwd)
            .filter(|s| refining && !s.content)
            .map(|s| s.hits.clone());
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
            sort: DirSort {
                key: SortKey::Relevance,
                reverse: false,
            },
            content,
            scored: matcher.is_scored(),
            matcher,
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
        if refining {
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
            "{} {}: {} found in {:.1} s{}{}",
            if s.content { "grep" } else { "search" },
            if cancelled { "cancelled" } else { "done" },
            crate::ui::thousands(s.hits.len() as u64),
            elapsed.as_secs_f32(),
            if errors > 0 {
                format!(" · {errors} unreadable")
            } else {
                String::new()
            },
            if s.truncated { " · capped" } else { "" },
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

    /// `,n` `,m` `,c` `,s` `,e` (capital: reversed), `,r`, `:sort …`: in search results,
    /// their order; in a folder, every folder's order (kept for next time).
    pub(super) fn set_sort(&mut self, key: SortKey, reverse: bool) {
        let sort = DirSort { key, reverse };
        if let Loc::Results { id, .. } = self.cwd {
            let Some(s) = self.search_mut(id) else { return };
            s.sort = sort;
            let label = s.sort_label();
            s.build();
            self.rebuild_view();
            return self.set_status(format!("results sorted by {label}"), false);
        }
        if key == SortKey::Relevance {
            return self.set_status(
                ",r sorts search results by relevance; here: ,n ,m ,c ,s ,e".into(),
                false,
            );
        }
        self.sort = sort;
        self.rebuild_view();
        // Saved once the keys settle (tick), so a burst of `,` keys can't race.
        self.sort_dirty = Some(Instant::now());
        self.set_status(
            format!(
                "sorted by {} ({}), folders first · ,n back to name",
                key.word(),
                sort.meaning()
            ),
            false,
        );
    }

    /// `cycle-sort` (an older keymap.toml binding): relevance → path → size → modified
    /// in search results.
    pub(super) fn cycle_sort(&mut self) {
        let Loc::Results { id, .. } = self.cwd else {
            return self.set_status(",m ,s ,e … sort a folder".into(), false);
        };
        let next = match self.search_mut(id).map(|s| s.sort.key) {
            Some(SortKey::Relevance) => SortKey::Name,
            Some(SortKey::Name) => SortKey::Size,
            Some(SortKey::Size) => SortKey::Modified,
            _ => SortKey::Relevance,
        };
        self.set_sort(next, false);
    }

    // ------------------------------------------------------------ match preview

    /// The file under the cursor in content results, and the matcher that found it.
    fn grep_target(&self) -> Option<(Loc, Arc<SearchMatcher>)> {
        let s = self.search_state(&self.cwd).filter(|s| s.content)?;
        let e = self.selected().filter(|e| !e.is_container())?;
        let loc = self.cwd.join(&e.name).ok()?;
        Some((loc, Arc::clone(&s.matcher)))
    }

    /// From the debounced preview: read the matches of the file under the cursor, unless
    /// they're already shown or on their way.
    pub(super) fn request_grep_preview(&mut self) {
        let Some((loc, matcher)) = self.grep_target() else {
            return;
        };
        if self.grep_preview.as_ref().is_some_and(|p| p.loc == loc) {
            return;
        }
        self.grep_preview = Some(GrepPreview {
            loc: loc.clone(),
            result: None,
            current: 0,
        });
        self.worker.grep_preview(loc, matcher);
    }

    pub(super) fn on_grep_preview(&mut self, loc: Loc, result: Result<MatchPreview, String>) {
        // A late answer for a file the cursor has left is dropped.
        if let Some(p) = self.grep_preview.as_mut().filter(|p| p.loc == loc) {
            p.result = Some(result);
            p.current = 0;
            self.dirty = true;
        }
    }

    /// The match preview for `loc`, if it's the one held.
    pub fn grep_preview_of(&self, loc: &Loc) -> Option<&GrepPreview> {
        self.grep_preview.as_ref().filter(|p| &p.loc == loc)
    }

    /// `]m` / `[m`: step `n` matches through the preview (no wrap, like vim's `]m`).
    pub(super) fn step_match(&mut self, n: usize, forward: bool) {
        let here = self.grep_target().map(|(loc, _)| loc);
        let Some(p) = self
            .grep_preview
            .as_mut()
            .filter(|p| here.as_ref() == Some(&p.loc))
        else {
            return self.set_status(
                "]m / [m step through the matches of a content search (S)".into(),
                false,
            );
        };
        let Some(Ok(preview)) = &p.result else { return };
        let last = preview.matches.len().saturating_sub(1);
        let target = if forward {
            p.current.saturating_add(n).min(last)
        } else {
            p.current.saturating_sub(n)
        };
        let edge = target == p.current;
        p.current = target;
        let total = preview.matches.len();
        let more = if preview.truncated { "+" } else { "" };
        let msg = match (edge, forward) {
            (true, true) => format!("last match ({total}{more})"),
            (true, false) => "first match".to_string(),
            _ => format!("match {} of {total}{more}", target + 1),
        };
        self.set_status(msg, false);
    }
}

/// The matches of one content-search result, shown in the preview column.
pub struct GrepPreview {
    pub loc: Loc,
    /// None while the file is being read.
    pub result: Option<Result<MatchPreview, String>>,
    /// Index into `MatchPreview::matches` of the match `]m` / `[m` is on.
    pub current: usize,
}

impl App {
    /// The folder order if it changed and isn't saved yet (written at exit by main).
    pub fn take_unsaved_sort(&mut self) -> Option<crate::state::UiState> {
        self.sort_dirty
            .take()
            .map(|_| crate::state::UiState::new(self.sort, crate::ui::theme::scheme()))
    }
}
