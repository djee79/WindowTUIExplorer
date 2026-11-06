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

use super::path::Loc;
use super::{Entry, EntryKind, Listing, sort_entries};
use crate::event::{AppEvent, EventTx};
use crate::platform::{self, DriveKind};
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
    Unsupported(String),
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
            ListError::Unsupported(m) => f.write_str(m),
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
    fn is_cancelled(&self) -> bool {
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
}

enum Batch {
    Entries(Vec<Entry>),
    Failed(io::Error),
    Done,
}

impl Worker {
    pub fn new(rt: Handle, tx: EventTx, timeouts: Timeouts) -> Self {
        Worker {
            rt,
            tx,
            timeouts,
            next_id: AtomicU64::new(1),
        }
    }

    fn new_request(&self) -> Request {
        Request {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            cancel: CancelToken::default(),
        }
    }

    /// Lists `loc`. Sends `ListProgress` events while reading and one `Listed` at the end.
    pub fn list(&self, loc: Loc) -> Request {
        let req = self.new_request();
        let (id, cancel, tx, timeouts) =
            (req.id, req.cancel.clone(), self.tx.clone(), self.timeouts);
        self.rt.spawn(async move {
            let result = match &loc {
                Loc::Root => list_root(timeouts).await,
                Loc::Server(_) => Err(ListError::Unsupported(
                    "listing a server's shares is not supported yet; type \\\\server\\share"
                        .to_string(),
                )),
                Loc::Fs(p) => {
                    let timeout = timeout_for(&loc, timeouts).await;
                    list_dir(id, &loc, p.to_os_path(), timeout, &cancel, &tx).await
                }
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

async fn list_root(t: Timeouts) -> Result<Vec<Entry>, ListError> {
    let call = tokio::task::spawn_blocking(platform::drives);
    let drives = match tokio::time::timeout(t.network, call).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return Err(ListError::Other(e.to_string())),
        Err(_) => return Err(ListError::TimedOut),
    };
    Ok(drives
        .into_iter()
        .map(|d| Entry {
            name: format!("{}:", d.letter),
            kind: EntryKind::Drive(d.kind),
            is_link: false,
            size: 0,
            modified: None,
            created: None,
            attrs: Default::default(),
        })
        .collect())
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
    })
}
