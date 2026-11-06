//! The recursive walk behind `s` (CLAUDE.md section 12, rule 7).
//!
//! A pool of walker threads sharing one queue of directories. Each directory is listed
//! with `platform::find_entries` (`FindFirstFileExW`, basic info + large fetch), which
//! `examples/enum_bench.rs` measured 38 % faster than `ignore::WalkParallel` on an SMB
//! share with big directories (warm, 16 threads; 32 threads were slower). Every thread
//! matches what it lists with its own scratch and sends hits in ~16 ms batches with a
//! progress snapshot, so results stream in and the counter moves even with no matches.
//!
//! Pruned while walking: `$RECYCLE.BIN`, `System Volume Information`, hidden/system
//! entries (unless `hidden:yes`), `!segment` excludes, `.ignore` patterns, and junctions /
//! symlinks (unless `follow:yes`). A `.ignore` file is only read when the listing shows
//! one, so there is no extra round trip per directory.
//!
//! A thread stuck in an SMB call can't be interrupted. Cancelling doesn't wait for it:
//! `SearchDone` goes out at once, and the stuck thread exits silently when the OS returns.

use super::SearchMatcher;
use super::name::{Candidate, NameMatcher, Scratch};
use crate::event::{AppEvent, EventTx};
use crate::fs::path::FsPath;
use crate::fs::worker::CancelToken;
use crate::platform::{self, Attrs, FindEntry};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

const SKIP: &[&str] = &["$RECYCLE.BIN", "System Volume Information"];
const HIT_BATCH: usize = 256;
const HIT_INTERVAL: Duration = Duration::from_millis(16);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// How often idle threads and the coordinator look at the cancel flag.
const POLL: Duration = Duration::from_millis(50);
/// `follow:yes` without `depth:` still stops here, so a junction loop can't run forever.
const FOLLOW_DEPTH_CAP: usize = 64;

/// One match, everything the results folder shows without another round trip.
#[derive(Debug, Clone)]
pub struct Hit {
    /// Relative to the search root, `\`-separated.
    pub rel: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub attrs: Attrs,
    pub score: u32,
}

/// Walker threads. SMB enumeration is latency-bound: 16 measured best on a NAS (32 was
/// slower). Locally the cores, capped the same.
pub fn default_threads(network: bool) -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    if network { 16 } else { cpus.clamp(2, 16) }
}

/// A directory waiting to be listed.
struct Job {
    dir: FsPath,
    /// Relative to the root; empty for the root itself.
    rel: String,
    depth: usize,
    /// `.ignore` matchers from the root down to this directory.
    ignores: Arc<Vec<Gitignore>>,
}

struct Queue {
    jobs: VecDeque<Job>,
    active: usize,
    done: bool,
    /// Walker threads still running (they flush their last batch before leaving).
    alive: usize,
}

struct Shared {
    id: u64,
    matcher: Arc<SearchMatcher>,
    cancel: CancelToken,
    tx: EventTx,
    scanned: AtomicU64,
    errors: AtomicU64,
    now: SystemTime,
    max_depth: Option<usize>,
    queue: Mutex<Queue>,
    wake: Condvar,
}

/// Starts the walk. Sends `SearchProgress` batches and exactly one `SearchDone`.
pub fn spawn(
    id: u64,
    root: FsPath,
    matcher: Arc<SearchMatcher>,
    threads: usize,
    cancel: CancelToken,
    tx: EventTx,
) {
    let q = matcher.query();
    let max_depth = match (q.depth, q.follow) {
        (Some(d), _) => Some(d),
        (None, true) => Some(FOLLOW_DEPTH_CAP),
        (None, false) => None,
    };
    let shared = Arc::new(Shared {
        id,
        matcher,
        cancel,
        tx,
        scanned: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        now: SystemTime::now(),
        max_depth,
        queue: Mutex::new(Queue {
            jobs: VecDeque::from([Job {
                dir: root,
                rel: String::new(),
                depth: 0,
                ignores: Arc::new(Vec::new()),
            }]),
            active: 0,
            done: false,
            alive: 0,
        }),
        wake: Condvar::new(),
    });
    let spawned = std::thread::Builder::new()
        .name(format!("sentier-search-{id}"))
        .spawn(move || coordinate(&shared, threads));
    if let Err(e) = spawned {
        tracing::error!(error = %e, "couldn't start the search thread");
    }
}

/// Starts the walker threads (detached) and reports the end: once the queue is drained and
/// every walker has flushed its last batch and left, or right away on cancel, without
/// waiting for a thread stuck in the OS.
fn coordinate(shared: &Arc<Shared>, threads: usize) {
    let start = Instant::now();
    let threads = threads.max(1);
    if let Ok(mut q) = shared.queue.lock() {
        q.alive = threads;
    }
    for n in 0..threads {
        let s = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name(format!("sentier-walk-{}-{n}", shared.id))
            .spawn(move || walker(&s));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "couldn't start a walker thread");
            if let Ok(mut q) = shared.queue.lock() {
                q.alive -= 1;
            }
        }
    }
    let Ok(mut q) = shared.queue.lock() else {
        return;
    };
    // With no walker at all, nobody would ever mark the queue done.
    if q.alive == 0 {
        q.done = true;
    }
    loop {
        let drained = q.done && q.alive == 0;
        if drained || shared.cancel.is_cancelled() {
            break;
        }
        q = match shared.wake.wait_timeout(q, POLL) {
            Ok((g, _)) => g,
            Err(_) => return,
        };
    }
    drop(q);
    let _ = shared.tx.send(AppEvent::SearchDone {
        id: shared.id,
        scanned: shared.scanned.load(Ordering::Relaxed),
        errors: shared.errors.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
        cancelled: shared.cancel.is_cancelled(),
    });
}

/// Per-thread state: matcher scratch and the batch being collected.
struct Walker<'a> {
    shared: &'a Shared,
    scratch: Scratch,
    batch: Vec<Hit>,
    current: String,
    last_hits: Instant,
    last_progress: Instant,
}

impl Walker<'_> {
    fn flush(&mut self) {
        let hits = std::mem::take(&mut self.batch);
        self.last_hits = Instant::now();
        self.last_progress = Instant::now();
        // After a cancel, SearchDone has gone out: stay quiet.
        if self.shared.cancel.is_cancelled() {
            return;
        }
        let _ = self.shared.tx.send(AppEvent::SearchProgress {
            id: self.shared.id,
            hits,
            scanned: self.shared.scanned.load(Ordering::Relaxed),
            current: self.current.clone(),
        });
    }

    fn maybe_flush(&mut self) {
        let due_hits = !self.batch.is_empty()
            && (self.batch.len() >= HIT_BATCH || self.last_hits.elapsed() >= HIT_INTERVAL);
        if due_hits || self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.flush();
        }
    }
}

fn next_job(shared: &Shared) -> Option<Job> {
    let mut q = shared.queue.lock().ok()?;
    loop {
        if shared.cancel.is_cancelled() || q.done {
            return None;
        }
        if let Some(job) = q.jobs.pop_front() {
            q.active += 1;
            return Some(job);
        }
        if q.active == 0 {
            q.done = true;
            shared.wake.notify_all();
            return None;
        }
        q = shared.wake.wait_timeout(q, POLL).ok()?.0;
    }
}

fn finish_job(shared: &Shared, subdirs: Vec<Job>) {
    let Ok(mut q) = shared.queue.lock() else {
        return;
    };
    q.jobs.extend(subdirs);
    q.active -= 1;
    if q.jobs.is_empty() && q.active == 0 {
        q.done = true;
    }
    shared.wake.notify_all();
}

fn walker(shared: &Shared) {
    let mut w = Walker {
        shared,
        scratch: Scratch::default(),
        batch: Vec::new(),
        current: String::new(),
        last_hits: Instant::now(),
        last_progress: Instant::now(),
    };
    while let Some(job) = next_job(shared) {
        let subdirs = visit_dir(&mut w, &job);
        finish_job(shared, subdirs);
        w.maybe_flush();
    }
    if !w.batch.is_empty() {
        w.flush();
    }
    // Only now can the coordinator say "done": every hit of this thread is sent.
    if let Ok(mut q) = shared.queue.lock() {
        q.alive -= 1;
        shared.wake.notify_all();
    }
}

/// Lists one directory, matches its entries, returns the subdirectories to walk.
fn visit_dir(w: &mut Walker, job: &Job) -> Vec<Job> {
    let shared = w.shared;
    let os = job.dir.os_string();
    let entries = match platform::find_entries(&os) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(dir = %job.dir.display(), error = %e, "search: can't list");
            shared.errors.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        }
    };
    if !job.rel.is_empty() {
        w.current.clone_from(&job.rel);
    }
    let ignores = with_local_ignore(&job.ignores, &os, &entries);
    let q = shared.matcher.query();
    let depth = job.depth + 1;
    let descend = shared.max_depth.is_none_or(|max| depth < max);
    let mut subdirs = Vec::new();
    for e in &entries {
        if shared.cancel.is_cancelled() {
            break;
        }
        let is_dir = e.attrs.directory;
        if SKIP.iter().any(|s| e.name.eq_ignore_ascii_case(s))
            || (!q.hidden && (e.attrs.hidden || e.attrs.system))
            || q.exclude.iter().any(|x| e.name.eq_ignore_ascii_case(x))
            || ignored(&ignores, &os, &e.name, is_dir)
        {
            continue;
        }
        shared.scanned.fetch_add(1, Ordering::Relaxed);
        let rel = if job.rel.is_empty() {
            e.name.clone()
        } else {
            format!("{}\\{}", job.rel, e.name)
        };
        if let Some(hit) = matched(w, &job.dir, e, &rel, is_dir) {
            w.batch.push(hit);
        }
        if is_dir && descend && (!e.reparse || q.follow) {
            match job.dir.join(&e.name) {
                Ok(dir) => subdirs.push(Job {
                    dir,
                    rel,
                    depth,
                    ignores: Arc::clone(&ignores),
                }),
                Err(err) => tracing::debug!(name = %e.name, error = %err, "search: odd name"),
            }
        }
        w.maybe_flush();
    }
    subdirs
}

/// Name search scores the entry; content search filters by name, then reads the file
/// (here, in the walker thread) and scores it by its number of matching lines.
fn matched(w: &mut Walker, dir: &FsPath, e: &FindEntry, rel: &str, is_dir: bool) -> Option<Hit> {
    let shared = w.shared;
    let cand = Candidate {
        rel,
        name: &e.name,
        is_dir,
        size: if is_dir { 0 } else { e.size },
        modified: e.modified,
    };
    let score = match &*shared.matcher {
        SearchMatcher::Name(m) => m.score(&cand, shared.now, &mut w.scratch)?,
        SearchMatcher::Content(m) => {
            if !m.passes(&cand, shared.now, &mut w.scratch) {
                return None;
            }
            let path = dir.join(&e.name).ok()?.os_string();
            match m.count(&path, &shared.cancel) {
                Ok(n) => n?,
                Err(err) => {
                    tracing::debug!(path, error = %err, "search: can't read");
                    shared.errors.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        }
    };
    Some(Hit {
        rel: rel.to_string(),
        is_dir,
        is_link: e.reparse,
        size: cand.size,
        modified: e.modified,
        created: e.created,
        attrs: e.attrs,
        score,
    })
}

/// The parent's `.ignore` matchers, plus this directory's own when its listing has one.
fn with_local_ignore(
    parent: &Arc<Vec<Gitignore>>,
    dir_os: &str,
    entries: &[FindEntry],
) -> Arc<Vec<Gitignore>> {
    let has_ignore = entries
        .iter()
        .any(|e| !e.attrs.directory && e.name.eq_ignore_ascii_case(".ignore"));
    if !has_ignore {
        return Arc::clone(parent);
    }
    let mut b = GitignoreBuilder::new(dir_os);
    if let Some(err) = b.add(format!("{dir_os}\\.ignore")) {
        tracing::debug!(dir = dir_os, error = %err, "search: bad .ignore");
    }
    match b.build() {
        Ok(gi) => {
            let mut all = (**parent).clone();
            all.push(gi);
            Arc::new(all)
        }
        Err(_) => Arc::clone(parent),
    }
}

/// Deepest `.ignore` wins, and a `!pattern` there re-includes, as in ripgrep.
fn ignored(ignores: &[Gitignore], dir_os: &str, name: &str, is_dir: bool) -> bool {
    if ignores.is_empty() {
        return false;
    }
    let path = std::path::PathBuf::from(format!("{dir_os}\\{name}"));
    for gi in ignores.iter().rev() {
        match gi.matched(&path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// Re-scores already-found hits with a new matcher (`s` again refines instantly while a
/// fresh walk runs). Blocking; the worker runs it on the blocking pool.
pub fn rescore(hits: Vec<Hit>, matcher: &NameMatcher) -> Vec<Hit> {
    let (mut s, now) = (Scratch::default(), SystemTime::now());
    hits.into_iter()
        .filter_map(|mut h| {
            let name = h.rel.rsplit('\\').next().unwrap_or(&h.rel).to_string();
            let cand = Candidate {
                rel: &h.rel,
                name: &name,
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified,
            };
            h.score = matcher.score(&cand, now, &mut s)?;
            Some(h)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::path::Loc;
    use crate::search::query::Query;
    use std::path::{Path, PathBuf};

    fn fs(p: &Path) -> FsPath {
        match Loc::parse(&p.to_string_lossy()) {
            Ok(Loc::Fs(f)) => f,
            other => panic!("{p:?}: {other:?}"),
        }
    }

    /// A small real tree in the temp dir.
    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("sentier-walk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for f in [
            r"hitiso\B-1001_rev3.pdf",
            r"hitiso\old\B-1001_rev2.pdf",
            r"hitrep\iso_B-1001_bom.csv",
            r"hitrep\notes.txt",
            r"$RECYCLE.BIN\deleted.pdf",
        ] {
            let p = root.join(f);
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&p, b"x");
        }
        root
    }

    /// Runs a search to completion and returns the sorted relative paths found.
    fn search(root: &Path, q: &str) -> (Vec<String>, u64) {
        let (hits, scanned) = search_hits(root, q, false);
        (hits.into_iter().map(|h| h.rel).collect(), scanned)
    }

    /// Runs a search to completion and returns the hits, sorted by path.
    fn search_hits(root: &Path, q: &str, content: bool) -> (Vec<Hit>, u64) {
        let Ok(query) = Query::parse(q) else {
            panic!("{q}")
        };
        let Ok(matcher) = SearchMatcher::new(&query, content) else {
            panic!("{q}")
        };
        let (tx, rx) = crate::event::channel();
        spawn(
            1,
            fs(root),
            Arc::new(matcher),
            4,
            CancelToken::default(),
            tx,
        );
        let mut found = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(AppEvent::SearchProgress { hits, .. }) => found.extend(hits),
                Ok(AppEvent::SearchDone { scanned, .. }) => {
                    found.sort_by(|a, b| a.rel.cmp(&b.rel));
                    return (found, scanned);
                }
                Ok(_) => {}
                Err(e) => panic!("search didn't finish: {e}"),
            }
        }
    }

    #[test]
    fn walks_matches_and_prunes() {
        let root = tree("a");
        let (pdfs, scanned) = search(&root, "*.pdf");
        assert_eq!(
            pdfs,
            [r"hitiso\B-1001_rev3.pdf", r"hitiso\old\B-1001_rev2.pdf"]
        );
        assert!(scanned >= 7, "dirs and files counted: {scanned}");
        let (no_old, _) = search(&root, "*.pdf !old");
        assert_eq!(no_old, [r"hitiso\B-1001_rev3.pdf"]);
        let (fuzzy, _) = search(&root, "b1001 iso");
        assert!(
            fuzzy.contains(&r"hitrep\iso_B-1001_bom.csv".to_string()),
            "{fuzzy:?}"
        );
        assert!(
            !fuzzy.iter().any(|p| p.contains("RECYCLE")),
            "skipped: {fuzzy:?}"
        );
        let (dirs, _) = search(&root, "type:d hit");
        // Fuzzy runs over the whole relative path, so `hitiso\old` matches "hit" too.
        assert_eq!(dirs, ["hitiso", r"hitiso\old", "hitrep"]);
        let (shallow, _) = search(&root, "*.pdf depth:2");
        assert_eq!(shallow, [r"hitiso\B-1001_rev3.pdf"], "depth limit");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn honours_ignore_files() {
        let root = tree("c");
        // Root .ignore drops CSVs and the old\ folder; a nested one re-includes a CSV.
        let _ = std::fs::write(root.join(".ignore"), "*.csv\nold/\n");
        let _ = std::fs::write(root.join(r"hitrep\keep.csv"), b"x");
        let _ = std::fs::write(root.join(r"hitrep\.ignore"), "!keep.csv\n");
        let (all, _) = search(&root, "ext:pdf,csv");
        assert_eq!(
            all,
            [r"hitiso\B-1001_rev3.pdf", r"hitrep\keep.csv"],
            "{all:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn content_search_reads_files_and_counts() {
        let root = tree("d");
        let _ = std::fs::write(root.join(r"hitrep\notes.txt"), "PIPE one\nnone\nPIPE two\n");
        let _ = std::fs::write(root.join(r"hitrep\fix.pmlmac"), "new PIPE\n");
        let _ = std::fs::write(root.join(r"hitrep\blob.txt"), b"PIPE\x00binary");
        let found = |q: &str| {
            let (hits, _) = search_hits(&root, q, true);
            hits.into_iter()
                .map(|h| (h.rel, h.score))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            found("PIPE"),
            [
                (r"hitrep\fix.pmlmac".to_string(), 1),
                (r"hitrep\notes.txt".to_string(), 2)
            ],
            "binary skipped, counts are matching lines"
        );
        assert_eq!(
            found("PIPE ext:pmlmac"),
            [(r"hitrep\fix.pmlmac".to_string(), 1)],
            "name filters apply first"
        );
        assert_eq!(found("pipe !hitrep"), [], "excluded subtree isn't read");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_stops_and_reports() {
        let root = tree("b");
        let Ok(query) = Query::parse("x") else {
            panic!()
        };
        let Ok(matcher) = SearchMatcher::new(&query, false) else {
            panic!()
        };
        let (tx, rx) = crate::event::channel();
        let cancel = CancelToken::default();
        cancel.cancel();
        spawn(9, fs(&root), Arc::new(matcher), 2, cancel, tx);
        let done = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(AppEvent::SearchDone { id, cancelled, .. }) => break (id, cancelled),
                Ok(_) => {}
                Err(e) => panic!("{e}"),
            }
        };
        assert_eq!(done, (9, true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rescore_refines_found_hits() {
        let hit = |rel: &str| Hit {
            rel: rel.to_string(),
            is_dir: false,
            is_link: false,
            size: 1,
            modified: None,
            created: None,
            attrs: Attrs::default(),
            score: 0,
        };
        let Ok(q) = Query::parse("rev3") else {
            panic!()
        };
        let Ok(m) = NameMatcher::new(&q) else {
            panic!()
        };
        let kept = rescore(
            vec![hit(r"a\B-1001_rev3.pdf"), hit(r"a\B-1001_rev2.pdf")],
            &m,
        );
        assert_eq!(kept.len(), 1);
        assert!(kept[0].score > 0);
    }
}
