//! The FS worker: every filesystem call the UI needs, run off the UI thread
//! (CLAUDE.md rules 1, 2 and 7).
//!
//! Each listing runs in two halves:
//! - a blocking reader (`spawn_blocking`) that calls `read_dir` and streams batches;
//! - an async supervisor that applies the **no-progress timeout**: if no batch arrives
//!   within the timeout, the listing is reported as unreachable and the reader is told
//!   to stop. A huge local directory that keeps producing entries never times out; a dead
//!   share that never answers does, after 500 ms (local) or 3 s (network).
//!
//! A reader stuck inside a syscall on a dead share can't be interrupted. It stays parked
//! on the blocking pool until the OS gives up, and its result is dropped. The UI never
//! waits on it.

use super::path::{FsPath, Loc};
use super::{Entry, EntryKind, Listing, natural_cmp, sort_entries};
use crate::event::{AppEvent, EventTx};
use crate::keymap;
use crate::locations;
use crate::pins::{self, PinStore};
use crate::platform::{self, DriveKind};
use crate::search::SearchMatcher;
use crate::search::content::MatchPreview;
use crate::search::walk::{self, Hit};
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

/// Entries per batch sent from the reader, and the longest it holds a partial batch.
const BATCH_SIZE: usize = 512;
const BATCH_INTERVAL: Duration = Duration::from_millis(100);
/// How often the UI hears "N entries so far" for a big directory.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// Locations reachability probe timeout (CLAUDE.md feature 2).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Reading a local file for the match preview: longer than a listing (the file may be
/// big), still bounded. Network files use the network timeout.
const GREP_PREVIEW_LOCAL: Duration = Duration::from_secs(2);
/// Context lines around each match, and the most matches a preview collects.
const GREP_CONTEXT: usize = 2;
const GREP_MAX_MATCHES: usize = 500;

/// A successful reachability probe.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    /// How long `metadata()` on the root took.
    pub rtt: Duration,
    /// Free bytes on the volume, if that answered too.
    pub free: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub local: Duration,
    pub network: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            local: Duration::from_millis(500),
            network: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    TimedOut,
    Unreachable(String),
    NotFound,
    AccessDenied,
    NotADirectory,
    Other(String),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::TimedOut => f.write_str("unreachable (timed out)"),
            ListError::Unreachable(m) => write!(f, "unreachable ({m})"),
            ListError::NotFound => f.write_str("not found"),
            ListError::AccessDenied => f.write_str("access denied"),
            ListError::NotADirectory => f.write_str("not a directory"),
            ListError::Other(m) => f.write_str(m),
        }
    }
}

impl From<io::Error> for ListError {
    fn from(e: io::Error) -> Self {
        // ERROR_BAD_NETPATH, ERROR_NETNAME_DELETED, ERROR_BAD_NET_NAME,
        // ERROR_NO_NETWORK, ERROR_HOST_UNREACHABLE, ERROR_NETWORK_UNREACHABLE.
        if let Some(53 | 64 | 67 | 1222 | 1231 | 1232) = e.raw_os_error() {
            return ListError::Unreachable(e.to_string());
        }
        match e.kind() {
            io::ErrorKind::NotFound => ListError::NotFound,
            io::ErrorKind::PermissionDenied => ListError::AccessDenied,
            io::ErrorKind::NotADirectory => ListError::NotADirectory,
            _ => ListError::Other(e.to_string()),
        }
    }
}

/// Cancels a request. Dropping the token does *not* cancel; call [`CancelToken::cancel`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A request in flight. Results carry the same `id`, so stale ones can be dropped.
#[derive(Debug)]
pub struct Request {
    pub id: u64,
    pub cancel: CancelToken,
}

pub struct Worker {
    rt: Handle,
    tx: EventTx,
    timeouts: Timeouts,
    next_id: AtomicU64,
    pins_save: mpsc::UnboundedSender<PinStore>,
}

enum Batch {
    Entries(Vec<Entry>),
    Failed(io::Error),
    Done,
}

impl Worker {
    pub fn new(rt: Handle, tx: EventTx, timeouts: Timeouts) -> Self {
        let (pins_save, pins_rx) = mpsc::unbounded_channel();
        rt.spawn(pins_writer(pins_rx, tx.clone()));
        Worker {
            rt,
            tx,
            timeouts,
            next_id: AtomicU64::new(1),
            pins_save,
        }
    }

    fn new_request(&self) -> Request {
        Request {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            cancel: CancelToken::default(),
        }
    }

    /// Lists `loc`. Sends `ListProgress` events while reading and one `Listed` at the end.
    /// For the Locations root use [`Worker::list_root`], which also needs the saved shares.
    pub fn list(&self, loc: Loc) -> Request {
        self.spawn_list(loc, Vec::new(), Vec::new())
    }

    /// Lists the Locations root: `pins` (pinned-folder entries, as given), drives (with
    /// labels and mapped targets), then `shares`.
    pub fn list_root(&self, shares: Vec<FsPath>, pins: Vec<Entry>) -> Request {
        self.spawn_list(Loc::Root, shares, pins)
    }

    fn spawn_list(&self, loc: Loc, shares: Vec<FsPath>, pins: Vec<Entry>) -> Request {
        let req = self.new_request();
        let (id, cancel, tx, timeouts) =
            (req.id, req.cancel.clone(), self.tx.clone(), self.timeouts);
        self.rt.spawn(async move {
            let result = match &loc {
                Loc::Root => list_root(timeouts, shares, pins).await,
                Loc::Server(server) => list_server(server.clone(), timeouts.network).await,
                Loc::Fs(p) => {
                    let timeout = timeout_for(&loc, timeouts).await;
                    list_dir(id, &loc, p.to_os_path(), timeout, &cancel, &tx).await
                }
                // The app owns search results; there's nothing to list.
                Loc::Results { .. } => Err(ListError::Other("search results".to_string())),
            };
            if cancel.is_cancelled() {
                return;
            }
            let result = result.map(|entries| {
                Arc::new(Listing {
                    entries,
                    loaded_at: Instant::now(),
                })
            });
            let _ = tx.send(AppEvent::Listed { id, loc, result });
        });
        req
    }

    /// Free space on the volume holding `loc`.
    pub fn disk_space(&self, loc: Loc) {
        let Some(path) = loc.as_fs().map(|p| p.os_string()) else {
            return;
        };
        let (tx, timeouts) = (self.tx.clone(), self.timeouts);
        self.rt.spawn(async move {
            let timeout = timeout_for(&loc, timeouts).await;
            let call = tokio::task::spawn_blocking(move || platform::disk_space(&path));
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(Ok(Ok(space))) => Some(space),
                _ => None,
            };
            let _ = tx.send(AppEvent::DiskSpace { loc, space: result });
        });
    }

    /// Reachability probe for a Locations entry: `metadata()` on its root under a 2 s
    /// timeout, plus free space when that answers. Sends one `Probed`.
    ///
    /// On a dead host the blocking call stays parked for ~21 s (measured by
    /// `examples/probe_netapi.rs`); the timeout reports long before that, and the
    /// parked thread's late result is dropped.
    pub fn probe(&self, loc: Loc) {
        let Some(path) = loc.as_fs().map(|p| p.os_string()) else {
            return;
        };
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                std::fs::metadata(&path)?;
                let rtt = start.elapsed();
                let free = platform::disk_space(&path).ok().map(|s| s.free);
                Ok::<_, io::Error>(Probe { rtt, free })
            });
            let result = match tokio::time::timeout(PROBE_TIMEOUT, call).await {
                Ok(Ok(Ok(p))) => Ok(p),
                Ok(Ok(Err(e))) => Err(ListError::from(e)),
                Ok(Err(e)) => Err(ListError::Other(e.to_string())),
                Err(_) => Err(ListError::TimedOut),
            };
            let _ = tx.send(AppEvent::Probed { loc, result });
        });
    }

    /// Loads the saved shares. Sends one `SharesLoaded`.
    ///
    /// `%APPDATA%` can be a redirected folder on a file server, so config reads get the
    /// network timeout like any other network I/O.
    pub fn load_shares(&self) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(|| match locations::store_path() {
                Some(path) => locations::load(&path).map_err(|e| e.to_string()),
                None => Ok(Vec::new()),
            });
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(r) => r.unwrap_or_else(|e| Err(e.to_string())),
                Err(_) => Err("timed out reading %APPDATA%".to_string()),
            };
            let _ = tx.send(AppEvent::SharesLoaded(result));
        });
    }

    /// Reads and builds `keymap.toml`. Sends one `KeymapLoaded`; no file sends nothing.
    pub fn load_keymap(&self) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(|| {
                let path = keymap::store_path()?;
                match std::fs::read_to_string(&path) {
                    Ok(text) => Some(Ok(keymap::from_toml(&text))),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => Some(Err(format!("keymap.toml: {e}"))),
                }
            });
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(Ok(Some(r))) => r,
                Ok(Ok(None)) => return,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("keymap.toml: timed out reading %APPDATA%".to_string()),
            };
            let _ = tx.send(AppEvent::KeymapLoaded(result));
        });
    }

    /// Saves the saved shares. Reports only failures, as `SharesSaveFailed`.
    pub fn save_shares(&self, shares: Vec<FsPath>) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || match locations::store_path() {
                Some(path) => locations::save(&path, &shares).map_err(|e| e.to_string()),
                None => Err("APPDATA is not set".to_string()),
            });
            if let Err(e) = call.await.unwrap_or_else(|e| Err(e.to_string())) {
                let _ = tx.send(AppEvent::SharesSaveFailed(e));
            }
        });
    }

    /// Loads pins.toml. Sends one `PinsLoaded` (network timeout: APPDATA may be on a
    /// share).
    pub fn load_pins(&self) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(|| match pins::store_path() {
                Some(path) => pins::load(&path),
                None => Ok((PinStore::default(), Vec::new())),
            });
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(r) => r.unwrap_or_else(|e| Err(e.to_string())),
                Err(_) => Err("pins.toml: timed out reading %APPDATA%".to_string()),
            };
            let _ = tx.send(AppEvent::PinsLoaded(result));
        });
    }

    /// Saves pins.toml. Reports only failures, as `PinsSaveFailed`. Saves go through one
    /// writer task, in order, so a later save always lands last.
    pub fn save_pins(&self, store: PinStore) {
        let _ = self.pins_save.send(store);
    }

    /// Starts a recursive name search under `root`. Hits stream in as `SearchProgress`.
    pub fn search(&self, id: u64, root: &FsPath, matcher: Arc<SearchMatcher>) -> CancelToken {
        let cancel = CancelToken::default();
        let threads = walk::default_threads(root.is_network());
        walk::spawn(
            id,
            root.clone(),
            matcher,
            threads,
            cancel.clone(),
            self.tx.clone(),
        );
        cancel
    }

    /// Re-scores found hits with a refined name query, off the UI thread, as one batch
    /// for search `id`. (Content hits aren't re-scored: that means reading every file
    /// again, which the fresh walk does anyway.)
    pub fn rescore(&self, id: u64, hits: Vec<Hit>, matcher: Arc<SearchMatcher>) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || match &*matcher {
                SearchMatcher::Name(m) => walk::rescore(hits, m),
                SearchMatcher::Content(_) => Vec::new(),
            });
            if let Ok(hits) = call.await {
                let _ = tx.send(AppEvent::SearchProgress {
                    id,
                    hits,
                    scanned: 0,
                    current: String::new(),
                });
            }
        });
    }

    /// The matching lines of one file, with context, for the preview of a content-search
    /// result. Sends one `GrepPreview`; the app drops it if the cursor has moved on.
    pub fn grep_preview(&self, loc: Loc, matcher: Arc<SearchMatcher>) {
        let Some(path) = loc.as_fs().map(|p| p.os_string()) else {
            return;
        };
        let (tx, timeouts) = (self.tx.clone(), self.timeouts);
        self.rt.spawn(async move {
            let t = timeout_for(&loc, timeouts).await;
            let timeout = if t == timeouts.network {
                t
            } else {
                GREP_PREVIEW_LOCAL.max(t)
            };
            let call = tokio::task::spawn_blocking(move || match &*matcher {
                SearchMatcher::Content(m) => m
                    .preview(&path, GREP_CONTEXT, GREP_MAX_MATCHES)
                    .map_err(|e| e.to_string()),
                SearchMatcher::Name(_) => Ok(MatchPreview::default()),
            });
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("timed out after {:.0} s", timeout.as_secs_f32())),
            };
            let _ = tx.send(AppEvent::GrepPreview { loc, result });
        });
    }

    /// Opens a file with its associated program (ShellExecute "open").
    pub fn open(&self, loc: Loc) {
        let Some(path) = loc.as_fs().map(|p| p.os_string()) else {
            return;
        };
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || platform::shell_open(&path));
            let result = match call.await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(AppEvent::Opened { loc, result });
        });
    }
}

async fn timeout_for(loc: &Loc, t: Timeouts) -> Duration {
    if loc.is_network() {
        return t.network;
    }
    // A mapped drive letter looks local but isn't. GetDriveTypeW only reads the
    // mount table, but it is still a Win32 call, so keep it on the blocking pool.
    if let Some(crate::fs::path::FsRoot::Drive(letter)) = loc.as_fs().map(|p| p.root()) {
        let letter = *letter;
        let kind = tokio::task::spawn_blocking(move || platform::drive_kind(letter)).await;
        if let Ok(DriveKind::Network) = kind {
            return t.network;
        }
    }
    t.local
}

/// Pinned folders first (in pin order), drives (in letter order), then saved shares (in
/// natural order). Not sorted with
/// `sort_entries`: the Locations screen shows these as sections.
async fn list_root(
    t: Timeouts,
    shares: Vec<FsPath>,
    pins: Vec<Entry>,
) -> Result<Vec<Entry>, ListError> {
    let call = tokio::task::spawn_blocking(|| {
        platform::drives()
            .into_iter()
            .map(|d| {
                // The label read goes to the volume, so only local disks are asked; a
                // mapped drive's target comes from the mapping table and never stalls.
                let detail = match d.kind {
                    DriveKind::Network => platform::mapped_target(d.letter),
                    DriveKind::Fixed | DriveKind::RamDisk => platform::volume_label(d.letter),
                    _ => None,
                };
                Entry::virtual_entry(
                    format!("{}:", d.letter),
                    EntryKind::Drive(d.kind),
                    detail.unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
    });
    let drives = match tokio::time::timeout(t.network, call).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return Err(ListError::Other(e.to_string())),
        Err(_) => return Err(ListError::TimedOut),
    };
    let mut saved: Vec<Entry> = shares
        .iter()
        .map(|p| Entry::virtual_entry(p.display(), EntryKind::Share, String::new()))
        .collect();
    saved.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    let mut entries = pins;
    entries.extend(drives);
    entries.extend(saved);
    Ok(entries)
}

/// The pins.toml writer: saves in the order asked, skipping to the newest store when
/// several are queued (each one is the whole store).
async fn pins_writer(mut rx: mpsc::UnboundedReceiver<PinStore>, tx: EventTx) {
    while let Some(mut store) = rx.recv().await {
        while let Ok(newer) = rx.try_recv() {
            store = newer;
        }
        let call = tokio::task::spawn_blocking(move || match pins::store_path() {
            Some(path) => pins::save(&path, &store),
            None => Err("APPDATA is not set".to_string()),
        });
        if let Err(e) = call.await.unwrap_or_else(|e| Err(e.to_string())) {
            let _ = tx.send(AppEvent::PinsSaveFailed(e));
        }
    }
}
/// A server node: its browsable disk shares through `NetShareEnum`.
async fn list_server(server: String, timeout: Duration) -> Result<Vec<Entry>, ListError> {
    let call = tokio::task::spawn_blocking(move || platform::shares(&server));
    let shares = match tokio::time::timeout(timeout, call).await {
        Ok(Ok(Ok(s))) => s,
        Ok(Ok(Err(e))) => {
            return Err(match ListError::from(e) {
                ListError::AccessDenied => ListError::Other(
                    "this server won't list its shares (access denied); \
                     type \\\\server\\share to open one directly"
                        .to_string(),
                ),
                other => other,
            });
        }
        Ok(Err(e)) => return Err(ListError::Other(e.to_string())),
        Err(_) => return Err(ListError::TimedOut),
    };
    let mut entries: Vec<Entry> = shares
        .into_iter()
        .map(|s| Entry::virtual_entry(s.name, EntryKind::Share, s.remark))
        .collect();
    sort_entries(&mut entries);
    Ok(entries)
}

async fn list_dir(
    id: u64,
    loc: &Loc,
    path: PathBuf,
    timeout: Duration,
    cancel: &CancelToken,
    tx: &EventTx,
) -> Result<Vec<Entry>, ListError> {
    let (btx, mut brx) = mpsc::channel::<Batch>(16);
    let reader_cancel = cancel.clone();
    tokio::task::spawn_blocking(move || read_dir_blocking(path, btx, reader_cancel));

    let mut entries = Vec::new();
    let mut last_progress = Instant::now();
    loop {
        match tokio::time::timeout(timeout, brx.recv()).await {
            // Returning drops `brx`, so the reader's next send fails and it stops.
            Err(_) => return Err(ListError::TimedOut),
            Ok(None) => return Err(ListError::Other("listing stopped unexpectedly".to_string())),
            Ok(Some(Batch::Failed(e))) => return Err(e.into()),
            Ok(Some(Batch::Done)) => break,
            Ok(Some(Batch::Entries(batch))) => {
                entries.extend(batch);
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    let _ = tx.send(AppEvent::ListProgress {
                        id,
                        loc: loc.clone(),
                        count: entries.len(),
                    });
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(ListError::Other("cancelled".to_string()));
        }
    }
    // Sorting 50k+ names takes a few ms; keep it off the async workers too.
    tokio::task::spawn_blocking(move || {
        sort_entries(&mut entries);
        entries
    })
    .await
    .map_err(|e| ListError::Other(e.to_string()))
}

fn read_dir_blocking(path: PathBuf, tx: mpsc::Sender<Batch>, cancel: CancelToken) {
    let rd = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => {
            let _ = tx.blocking_send(Batch::Failed(e));
            return;
        }
    };
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut last_send = Instant::now();
    for item in rd {
        if cancel.is_cancelled() {
            return;
        }
        match item {
            Ok(de) => {
                if let Some(e) = make_entry(&de) {
                    batch.push(e);
                }
            }
            Err(e) => tracing::debug!(?path, error = %e, "skipping unreadable entry"),
        }
        if batch.len() >= BATCH_SIZE || last_send.elapsed() >= BATCH_INTERVAL {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_SIZE));
            if tx.blocking_send(Batch::Entries(full)).is_err() {
                return; // supervisor gone (timed out or cancelled)
            }
            last_send = Instant::now();
        }
    }
    if !batch.is_empty() && tx.blocking_send(Batch::Entries(batch)).is_err() {
        return;
    }
    let _ = tx.blocking_send(Batch::Done);
}

fn make_entry(de: &std::fs::DirEntry) -> Option<Entry> {
    let name = de.file_name().to_string_lossy().into_owned();
    // On Windows this metadata comes from the directory enumeration itself (no extra
    // round trip), and it describes the link, not its target. We never follow links
    // while listing: a junction pointing at a dead share must not stall the listing.
    let meta = de.metadata().ok()?;
    let attrs = platform::attrs(&meta);
    let is_link = meta.file_type().is_symlink();
    let kind = if attrs.directory || meta.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    Some(Entry {
        name,
        kind,
        is_link,
        size: if kind == EntryKind::File {
            meta.len()
        } else {
            0
        },
        modified: meta.modified().ok(),
        created: meta.created().ok(),
        attrs,
        detail: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `:e <file>` and a file as the start path rely on this: listing a file must come
    /// back as NotADirectory (Win32 ERROR_DIRECTORY, 267), not as a generic error.
    #[test]
    fn read_dir_of_a_file_is_not_a_directory() {
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let err = std::fs::read_dir(&file).err().map(ListError::from);
        assert_eq!(err, Some(ListError::NotADirectory), "{err:?}");
    }
}
