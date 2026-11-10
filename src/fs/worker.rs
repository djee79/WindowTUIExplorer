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
use crate::auth::Secret;
use crate::config;
use crate::event::{AppEvent, EventTx};
use crate::keymap;
use crate::locations;
use crate::pins::{self, PinStore};
use crate::platform::{self, DriveKind};
use crate::preview::{self, ImageKey};
use crate::search::SearchMatcher;
use crate::search::content::MatchPreview;
use crate::search::walk::{self, Hit};
use ratatui_image::picker::Picker;
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
/// Reading + encoding a picture for the preview column.
const IMAGE_PREVIEW_LOCAL: Duration = Duration::from_secs(5);
const IMAGE_PREVIEW_NETWORK: Duration = Duration::from_secs(15);
/// Connecting with credentials: a dead host takes ~20 s to give up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
/// The port-445 probe, name lookup included.
const MACHINE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// NetWkstaUserEnum on a host that answered the probe.
const MACHINE_USERS_TIMEOUT: Duration = Duration::from_secs(8);
/// `machines.source_cmd` (e.g. a directory query).
const SOURCE_CMD_TIMEOUT: Duration = Duration::from_secs(20);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// The server didn't accept the login (1326, 86, 1244, 1909…): the code.
    LogonFailure(u32),
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
            ListError::LogonFailure(code) => f.write_str(&crate::auth::error_message(*code)),
            ListError::NotADirectory => f.write_str("not a directory"),
            ListError::Other(m) => f.write_str(m),
        }
    }
}

impl From<io::Error> for ListError {
    fn from(e: io::Error) -> Self {
        // ERROR_BAD_NETPATH, ERROR_NETNAME_DELETED, ERROR_BAD_NET_NAME,
        // ERROR_NO_NETWORK, ERROR_HOST_UNREACHABLE, ERROR_NETWORK_UNREACHABLE.
        if let Some(code @ (86 | 1244 | 1326 | 1330 | 1331 | 1909)) = e.raw_os_error() {
            return ListError::LogonFailure(code as u32);
        }
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

/// What `Worker::machine_users` found out about who uses a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineUsers {
    /// `NetWkstaUserEnum`, people only.
    pub logged_on: Vec<String>,
    /// The profile folders of `C$\Users`, newest first.
    pub profiles: Vec<String>,
    /// The newest profile that isn't a system or admin one.
    pub likely: Option<String>,
}

/// The round trip of a TCP connect to `host`:445, with a 500 ms timeout per address.
/// Blocking (name lookup + connect).
fn port_445(host: &str) -> Result<Duration, String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let addrs: Vec<_> = (host, 445)
        .to_socket_addrs()
        .map_err(|_| "name not found".to_string())?
        .collect();
    let mut last = "no address".to_string();
    for addr in addrs {
        let start = Instant::now();
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(_) => return Ok(start.elapsed()),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                last = "port 445 doesn't answer".to_string()
            }
            Err(_) => last = "port 445 refused".to_string(),
        }
    }
    Err(last)
}

/// A UNC path, or a drive letter Windows says is a network drive. Blocking.
fn no_recycle_bin(path: &str) -> bool {
    if path.starts_with(r"\\") {
        return true;
    }
    let b = path.as_bytes();
    b.len() >= 2
        && b[1] == b':'
        && b[0].is_ascii_alphabetic()
        && platform::drive_kind((b[0] as char).to_ascii_uppercase()) == platform::DriveKind::Network
}

/// `%LOCALAPPDATA%\sentier\audit.log` (a temp file in tests).
pub fn audit_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join("sentier-test-audit.log"));
    }
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("sentier")
            .join("audit.log"),
    )
}

fn append_audit(line: &str) {
    use std::io::Write;
    let Some(path) = audit_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        Err(e) => tracing::warn!(error = %e, "writing the audit log"),
    }
}

/// Runs `cmd` through `cmd.exe /D /C`, hidden (no console window), and returns what it
/// printed. Killed after `limit`. Its stderr goes to the log, never the screen (rule 6).
/// Blocking.
fn run_source_cmd(cmd: &str, limit: Duration) -> Result<String, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = Command::new("cmd")
        .args(["/D", "/C"])
        // As typed: cmd.exe has its own quoting rules, so Rust must not escape quotes.
        .raw_arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    // Read both pipes on their own threads so a chatty command can't block on a full pipe.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(p) = stdout.as_mut() {
            let _ = p.read_to_end(&mut s);
        }
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(p) = stderr.as_mut() {
            let _ = p.read_to_end(&mut s);
        }
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let out = out.join().unwrap_or_default();
    let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).to_string();
    if !err.trim().is_empty() {
        tracing::info!(stderr = %err.trim(), "source_cmd wrote to stderr");
    }
    if !status.success() {
        let first = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        return Err(format!(
            "exited with {}{}",
            status.code().unwrap_or(-1),
            if first.is_empty() {
                String::new()
            } else {
                format!(": {first}")
            }
        ));
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

/// How to connect: plainly, or with one of the two fixes for error 1219 (already connected
/// to that server as another user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectHow {
    Plain,
    /// Drop every connection to the server first (mapped drives come back at next logon).
    DropOthersFirst,
    /// Go through the server's IP address, which Windows treats as another server.
    ViaAlias,
}

/// The server's IPv4 address, for connecting through an alias. A blocking DNS lookup.
fn server_ip(server: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    (server, 445)
        .to_socket_addrs()
        .ok()?
        .find(|a| a.is_ipv4())
        .map(|a| a.ip().to_string())
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
    /// config.toml's `preview.max_network_mb`, in bytes.
    preview_limit: Arc<AtomicU64>,
}

enum Batch {
    Entries(Vec<Entry>),
    Failed(io::Error),
    Done,
}

impl Worker {
    pub fn new(rt: Handle, tx: EventTx, timeouts: Timeouts) -> Self {
        let (pins_save, pins_rx) = mpsc::unbounded_channel();
        let own_write = Arc::new(AtomicU64::new(0));
        rt.spawn(pins_writer(pins_rx, tx.clone(), own_write.clone()));
        // Tests never watch the user's file (store_path is a temp file there anyway).
        if !cfg!(test) {
            rt.spawn(pins_watcher(tx.clone(), own_write));
        }
        Worker {
            rt,
            tx,
            timeouts,
            next_id: AtomicU64::new(1),
            pins_save,
            preview_limit: Arc::new(AtomicU64::new(preview::MAX_NETWORK_BYTES)),
        }
    }

    /// config.toml's `preview.max_network_mb`: network files bigger than this aren't
    /// previewed until `zp`.
    pub fn set_preview_limit(&self, mb: u64) {
        self.preview_limit
            .store(mb.max(1).saturating_mul(1024 * 1024), Ordering::Relaxed);
    }

    /// A callback for matcher threads (the picker's nucleo pool): wakes the UI loop.
    pub fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let tx = self.tx.clone();
        Arc::new(move || {
            let _ = tx.send(AppEvent::Wake);
        })
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
                // The app owns search results and machine views; nothing to list.
                Loc::Results { .. } => Err(ListError::Other("search results".to_string())),
                Loc::Machines | Loc::Machine(_) => Err(ListError::Other("machines".to_string())),
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

    /// Reads config.toml. Sends one `ConfigLoaded`; no file sends nothing.
    pub fn load_config(&self) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(|| {
                let path = config::store_path()?;
                match std::fs::read_to_string(&path) {
                    Ok(text) => Some(config::parse(&text)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => Some(Err(format!("config.toml: {e}"))),
                }
            });
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(Ok(Some(r))) => r,
                Ok(Ok(None)) => return,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("config.toml: timed out reading %APPDATA%".to_string()),
            };
            let _ = tx.send(AppEvent::ConfigLoaded(result.map(Box::new)));
        });
    }

    /// Connects to `target` (`\\server\share` or `\\server\IPC$`) as `user`, `how` says
    /// whether to drop the server's other connections first or go through an alias (the
    /// two fixes for error 1219). With `remember`, also writes the credential to Windows
    /// Credential Manager for the server, once the connection worked. Sends one
    /// `Connected`. The password copy lives in this task only and is wiped when it ends.
    pub fn net_connect(
        &self,
        target: String,
        user: String,
        password: Secret,
        remember: bool,
        how: ConnectHow,
    ) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let (t, u) = (target.clone(), user.clone());
            let call = tokio::task::spawn_blocking(move || {
                let server = crate::auth::server_of(&t).to_string();
                let mut dropped = Vec::new();
                let actual = match how {
                    ConnectHow::Plain => t.clone(),
                    ConnectHow::DropOthersFirst => {
                        let live = platform::net_connections().unwrap_or_default();
                        for c in live {
                            if !crate::auth::server_of(&c.remote).eq_ignore_ascii_case(&server) {
                                continue;
                            }
                            // A mapped drive is dropped by its letter; it comes back at the
                            // next logon (the profile isn't updated).
                            let name = c.local.clone().unwrap_or_else(|| c.remote.clone());
                            if platform::net_disconnect(&name).is_ok() {
                                dropped.push(name);
                            }
                        }
                        t.clone()
                    }
                    ConnectHow::ViaAlias => match server_ip(&server) {
                        Some(ip) => crate::auth::with_server(&t, &ip),
                        None => return (t.clone(), Err(53), None, dropped),
                    },
                };
                let result = platform::net_connect(&actual, Some(&u), Some(password.with_nul()));
                let remembered = (remember && result.is_ok()).then(|| {
                    platform::cred_write(
                        crate::auth::server_of(&actual),
                        &u,
                        password.without_nul(),
                    )
                    .map_err(|e| e.to_string())
                });
                (actual, result, remembered, dropped)
            });
            let (actual, result, remembered, dropped) =
                match tokio::time::timeout(CONNECT_TIMEOUT, call).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "connect task failed");
                        (target.clone(), Err(0), None, Vec::new())
                    }
                    // ERROR_SEM_TIMEOUT: "the semaphore timeout period has expired".
                    Err(_) => (target.clone(), Err(121), None, Vec::new()),
                };
            let _ = tx.send(AppEvent::Connected {
                requested: target,
                target: actual,
                user,
                result,
                remembered,
                dropped,
            });
        });
    }

    /// Is `host` up? A TCP connect to port 445 (SMB) with a 500 ms timeout, before any SMB
    /// call, so an asleep laptop answers in half a second instead of a 20 s SMB timeout.
    /// Sends one `MachineProbe` with the round trip.
    pub fn machine_probe(&self, host: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let h = host.clone();
            let call = tokio::task::spawn_blocking(move || port_445(&h));
            let result = match tokio::time::timeout(MACHINE_PROBE_TIMEOUT, call).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("no answer (name lookup timed out)".to_string()),
            };
            let _ = tx.send(AppEvent::MachineProbe { host, result });
        });
    }

    /// Does the server of `open` (a `\\server\share…` path) answer on port 445? When a
    /// share won't list, an answer means the server is up and the login is the problem.
    /// Sends one `SmbAnswer`.
    pub fn smb_answers(&self, open: String) {
        let tx = self.tx.clone();
        let server = crate::auth::server_of(&open).to_string();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || port_445(&server));
            let up = matches!(
                tokio::time::timeout(MACHINE_PROBE_TIMEOUT, call).await,
                Ok(Ok(Ok(_)))
            );
            let _ = tx.send(AppEvent::SmbAnswer { open, up });
        });
    }

    /// Who uses `host`: `NetWkstaUserEnum` (needs admin rights), and the profile folders
    /// of `\\host\C$\Users` (newest first) as the fallback and the choice for `u`.
    /// Sends one `MachineUsers`.
    pub fn machine_users(&self, host: String) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let h = host.clone();
            let logged = tokio::task::spawn_blocking(move || platform::logged_on_users(&h));
            let logged_on = match tokio::time::timeout(MACHINE_USERS_TIMEOUT, logged).await {
                Ok(Ok(Ok(u))) => crate::machines::people(&u),
                _ => Vec::new(),
            };
            let h = host.clone();
            let profiles = tokio::task::spawn_blocking(move || {
                let dir = format!(r"\\{h}\C$\Users");
                let mut out = Vec::new();
                for e in std::fs::read_dir(&dir)?.flatten() {
                    let Ok(meta) = e.metadata() else { continue };
                    if meta.is_dir() {
                        out.push((
                            e.file_name().to_string_lossy().to_string(),
                            meta.modified().ok(),
                        ));
                    }
                }
                Ok::<_, io::Error>(out)
            });
            let profiles = match tokio::time::timeout(timeout, profiles).await {
                Ok(Ok(Ok(p))) => Ok(p),
                Ok(Ok(Err(e))) => Err(ListError::from(e).to_string()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timed out".to_string()),
            };
            let result = match profiles {
                Ok(p) => {
                    let likely = crate::machines::likely_user(&p);
                    let mut names: Vec<(String, Option<std::time::SystemTime>)> = p;
                    names.sort_by_key(|n| std::cmp::Reverse(n.1));
                    Ok(MachineUsers {
                        logged_on,
                        profiles: names.into_iter().map(|(n, _)| n).collect(),
                        likely,
                    })
                }
                // Logged-on users alone still say who it is.
                Err(_) if !logged_on.is_empty() => Ok(MachineUsers {
                    logged_on,
                    profiles: Vec::new(),
                    likely: None,
                }),
                Err(e) => Err(e),
            };
            let _ = tx.send(AppEvent::MachineUsers { host, result });
        });
    }

    /// Which of `paths` (expanded templates) exist. Sends one `PathsChecked`.
    pub fn check_paths(&self, host: String, paths: Vec<String>) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .map(|p| {
                        let there = std::fs::metadata(&p).is_ok_and(|m| m.is_dir());
                        (p, there)
                    })
                    .collect::<Vec<_>>()
            });
            let found = match tokio::time::timeout(timeout * 2, call).await {
                Ok(Ok(f)) => f,
                _ => Vec::new(),
            };
            let _ = tx.send(AppEvent::PathsChecked { host, found });
        });
    }

    /// The machine picker's slow sources: the inventory CSV (read under the network
    /// timeout: it may be on a share) and `source_cmd` (run hidden, killed after 20 s).
    /// Sends one `MachineSources` with each source's machines or why not.
    pub fn machine_sources(&self, csv: Option<String>, cmd: Option<String>) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let mut results: Vec<(
                &'static str,
                Result<Vec<crate::machines::MachineRecord>, String>,
            )> = Vec::new();
            if let Some(path) = csv {
                let read = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path));
                let r = match tokio::time::timeout(timeout * 2, read).await {
                    Ok(Ok(Ok(text))) => Ok(crate::machines::parse_csv(&text)),
                    Ok(Ok(Err(e))) => Err(format!("inventory CSV: {e}")),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("inventory CSV: timed out".to_string()),
                };
                results.push(("inventory", r));
            }
            if let Some(cmd) = cmd {
                let run =
                    tokio::task::spawn_blocking(move || run_source_cmd(&cmd, SOURCE_CMD_TIMEOUT));
                let r = match run.await {
                    Ok(Ok(out)) => {
                        crate::machines::parse_json(&out).map_err(|e| format!("source_cmd: {e}"))
                    }
                    Ok(Err(e)) => Err(format!("source_cmd: {e}")),
                    Err(e) => Err(e.to_string()),
                };
                results.push(("command", r));
            }
            let _ = tx.send(AppEvent::MachineSources(results));
        });
    }

    /// Loads the recent machines. Sends one `RecentMachines`.
    pub fn load_recent_machines(&self, days: u32) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || match crate::machines::recent_path() {
                Some(p) => crate::machines::load_recent(&p, days),
                None => Ok(Vec::new()),
            });
            let result = match tokio::time::timeout(Duration::from_secs(3), call).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("machines.tsv: timed out".to_string()),
            };
            let _ = tx.send(AppEvent::RecentMachines(result));
        });
    }

    /// Saves the recent machines (local disk). Failures are only logged.
    pub fn save_recent_machines(&self, list: Vec<crate::machines::Recent>) {
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || match crate::machines::recent_path() {
                Some(p) => crate::machines::save_recent(&p, &list),
                None => Ok(()),
            });
            if let Ok(Ok(Err(e))) = tokio::time::timeout(Duration::from_secs(3), call).await {
                tracing::warn!(error = %e, "couldn't save recent machines");
            }
        });
    }

    /// `:net list`: every live connection. Sends one `NetList`.
    pub fn net_list(&self) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(platform::net_connections);
            let result = match tokio::time::timeout(Duration::from_secs(5), call).await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => Err(0),
                Err(_) => Err(121),
            };
            let _ = tx.send(AppEvent::NetList(result));
        });
    }

    /// `:net drop`: disconnects each remote name. Sends one `NetDropped` with each result.
    pub fn net_drop(&self, names: Vec<String>) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || {
                names
                    .into_iter()
                    .map(|n| {
                        let r = platform::net_disconnect(&n);
                        (n, r)
                    })
                    .collect::<Vec<_>>()
            });
            let results = match tokio::time::timeout(CONNECT_TIMEOUT, call).await {
                Ok(Ok(r)) => r,
                _ => Vec::new(),
            };
            let _ = tx.send(AppEvent::NetDropped(results));
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

    /// `:pins restore`: the backups of pins.toml, each read and compared with the current
    /// file. One `PinBackups` (network timeout: APPDATA may be on a share).
    pub fn list_pin_backups(&self) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(|| {
                pins::store_path()
                    .map(|p| crate::pin_backups::listed(&p))
                    .unwrap_or_default()
            });
            let found = match tokio::time::timeout(timeout, call).await {
                Ok(Ok(found)) => found,
                _ => Vec::new(),
            };
            let _ = tx.send(AppEvent::PinBackups(found));
        });
    }

    /// Puts a backup in place of pins.toml (the current file is backed up first). One
    /// `PinsRestored`; `when` is the backup's time, for the message.
    pub fn restore_pins(&self, backup: std::path::PathBuf, when: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let call = tokio::task::spawn_blocking(move || {
                let path = pins::store_path().ok_or("APPDATA is not set")?;
                let now = chrono::Local::now().naive_local();
                crate::pin_backups::restore(&path, &backup, now)
            });
            let result = call
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
                .map(|store| (store, when));
            let _ = tx.send(AppEvent::PinsRestored(result));
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

    /// The first [`crate::preview::text::READ_BYTES`] of a file, decoded for the text
    /// preview: one `TextPreview` (None in it: not text). Same timeouts as the match
    /// preview; only that much is read, so a big log on a share stays quick.
    pub fn text_preview(&self, loc: Loc, kind: crate::preview::text::TextKind) {
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
            let call = tokio::task::spawn_blocking(move || -> Result<_, String> {
                use std::io::Read;
                let f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                let mut buf = Vec::new();
                f.take(crate::preview::text::READ_BYTES as u64)
                    .read_to_end(&mut buf)
                    .map_err(|e| e.to_string())?;
                let whole = buf.len() as u64 >= size;
                Ok(crate::preview::text::decode(&buf, whole, kind))
            });
            let result = match tokio::time::timeout(timeout, call).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("timed out after {:.0} s", timeout.as_secs_f32())),
            };
            let _ = tx.send(AppEvent::TextPreview {
                loc,
                result: result.map(|o| o.map(Box::new)),
            });
        });
    }

    /// Reads and encodes the picture behind `key` for the preview column. Sends one
    /// `ImagePreview`. Network files over 50 MB aren't read (mapped drives count as
    /// network). Reading the file gets 5 s locally, 15 s on the network; decoding and
    /// encoding come after, untimed (CPU only: slow on a huge picture, but it can't
    /// hang). A late answer for a file the cursor has left is dropped by the app.
    pub fn image_preview(&self, key: ImageKey, picker: Picker) {
        let Some(path) = key.loc.as_fs().map(|p| p.os_string()) else {
            return;
        };
        let (tx, timeouts) = (self.tx.clone(), self.timeouts);
        let limit = self.preview_limit.load(Ordering::Relaxed);
        self.rt.spawn(async move {
            let network = timeout_for(&key.loc, timeouts).await == timeouts.network;
            if preview::too_big_for_network(key.size, network, limit) {
                let mb = key.size / (1024 * 1024);
                let result = Err(format!(
                    "large file on the network ({mb} MB): not previewed"
                ));
                let _ = tx.send(AppEvent::ImagePreview { key, result });
                return;
            }
            // Shell thumbnails read the file themselves: the whole call is timed, with the
            // usual network timeout (CLAUDE.md section 11).
            let kind = preview::PreviewKind::of(&key.loc.name());
            if kind == Some(preview::PreviewKind::Thumbnail) {
                let timeout = if network {
                    timeouts.network
                } else {
                    IMAGE_PREVIEW_LOCAL
                };
                let (cols, rows, view) = (key.cols, key.rows, key.view);
                let call = tokio::task::spawn_blocking(move || {
                    preview::image::thumbnail(&path, &picker, cols, rows, view).map(Arc::new)
                });
                let result = match tokio::time::timeout(timeout, call).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err(format!("thumbnail timed out after {} s", timeout.as_secs())),
                };
                let _ = tx.send(AppEvent::ImagePreview { key, result });
                return;
            }
            let timeout = if network {
                IMAGE_PREVIEW_NETWORK
            } else {
                IMAGE_PREVIEW_LOCAL
            };
            let read = tokio::task::spawn_blocking(move || preview::image::read(&path));
            let read = match tokio::time::timeout(timeout, read).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("timed out reading after {} s", timeout.as_secs())),
            };
            let result = match read {
                Ok(bytes) => {
                    let (cols, rows, page, view) = (key.cols, key.rows, key.page, key.view);
                    let is_pdf = preview::PreviewKind::of(&key.loc.name())
                        == Some(preview::PreviewKind::Pdf);
                    let decode = tokio::task::spawn_blocking(move || {
                        if is_pdf {
                            preview::image::pdf(&bytes, page, &picker, cols, rows, view)
                        } else {
                            preview::image::decode(&bytes, &picker, cols, rows, view)
                        }
                        .map(Arc::new)
                    });
                    decode.await.unwrap_or_else(|e| Err(e.to_string()))
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(AppEvent::ImagePreview { key, result });
        });
    }

    /// Runs a copy / move job on the blocking pool; events come back as `Job`. The
    /// returned control cancels, pauses, skips a locked file or retries it now.
    pub fn start_job(&self, id: u64, spec: crate::jobs::run::Spec) -> crate::jobs::Control {
        let ctl = crate::jobs::Control::default();
        let (tx, c) = (self.tx.clone(), ctl.clone());
        self.rt.spawn_blocking(move || {
            crate::jobs::run::run(&spec, &c, &|ev| {
                let _ = tx.send(AppEvent::Job { id, ev });
            });
        });
        ctl
    }

    /// Runs a backup preset's pipeline on the blocking pool; events come back as `Job`.
    pub fn start_backup(
        &self,
        id: u64,
        spec: crate::jobs::zip::BackupSpec,
    ) -> crate::jobs::Control {
        let ctl = crate::jobs::Control::default();
        let (tx, c) = (self.tx.clone(), ctl.clone());
        self.rt.spawn_blocking(move || {
            crate::jobs::zip::run(&spec, &c, &|ev| {
                let _ = tx.send(AppEvent::Job { id, ev });
            });
        });
        ctl
    }

    /// The folder `archive` extracts into (`jobs::archive::target_folder`) and whether
    /// `dest_dir` has it already: one `ArchiveTarget`. Reading a big zip's directory
    /// over SMB can take a moment; 20 s at most.
    pub fn archive_target(
        &self,
        token: u64,
        archive: String,
        dest_dir: String,
        seven: Option<std::path::PathBuf>,
    ) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let read = tokio::task::spawn_blocking(move || {
                let folder = crate::jobs::archive::target_folder(&archive, seven.as_deref())?;
                let path = std::path::Path::new(&dest_dir).join(&folder);
                Ok((folder, path.exists()))
            });
            let result = match tokio::time::timeout(Duration::from_secs(20), read).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("reading the archive timed out".to_string()),
            };
            let _ = tx.send(AppEvent::ArchiveTarget { token, result });
        });
    }

    /// A compress / extract job on the blocking pool; events come back as `Job`. With
    /// `audit` (an admin share's machine and user) the outcome goes to `audit.log`.
    pub fn start_archive(
        &self,
        id: u64,
        job: crate::jobs::archive::ArchiveJob,
        audit: Option<(String, Option<String>)>,
    ) -> crate::jobs::Control {
        let ctl = crate::jobs::Control::default();
        let (tx, c) = (self.tx.clone(), ctl.clone());
        self.rt.spawn_blocking(move || {
            let outcome = std::cell::Cell::new("ok");
            crate::jobs::archive::run(&job, &c, &|ev| {
                match &ev {
                    crate::jobs::JobEvent::Error(_) => outcome.set("FAILED"),
                    crate::jobs::JobEvent::Finished { cancelled: true } => outcome.set("cancelled"),
                    _ => {}
                }
                let _ = tx.send(AppEvent::Job { id, ev });
            });
            if let Some((host, user)) = audit {
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                append_audit(&format!(
                    "{now}\t{host}\t{}\t{}\t{}\t{}",
                    user.as_deref().unwrap_or("-"),
                    job.describe(),
                    outcome.get(),
                    job.paths().join(" | ")
                ));
            }
        });
        ctl
    }

    /// A sync preset's dry run on the blocking pool: one `SyncPreviewed`. No timeout:
    /// robocopy gives up on an unreachable share by itself (`/R:0`), and the cancel
    /// token (Esc) kills it.
    pub fn sync_preview(
        &self,
        token: u64,
        steps: Vec<crate::jobs::robocopy::Step>,
        opts: crate::jobs::robocopy::Options,
    ) -> CancelToken {
        let cancel = CancelToken::default();
        let (tx, c) = (self.tx.clone(), cancel.clone());
        self.rt.spawn_blocking(move || {
            let result = crate::jobs::run::dry_run_steps(&steps, &opts, &c)
                .map(|(t, code)| (Box::new(t), code));
            if !c.is_cancelled() {
                let _ = tx.send(AppEvent::SyncPreviewed { token, result });
            }
        });
        cancel
    }

    // -------------------------------------------------------------- open with (M5)

    /// Reads open.toml, finds every opener's program, and the last-used openers: one
    /// `OpenConfig`. No file: the built-in rules.
    pub fn load_open_config(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            use crate::open::{rules, run};
            let (cfg, problems) = match run::config_path().map(std::fs::read_to_string) {
                Some(Ok(text)) => rules::parse(&text),
                Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => (
                    rules::OpenConfig::default(),
                    vec![format!("open.toml: {e}")],
                ),
                _ => (rules::OpenConfig::default(), Vec::new()),
            };
            let resolved = run::resolve_all(&cfg);
            let last = run::load_last();
            let _ = tx.send(AppEvent::OpenConfig {
                cfg: Box::new(cfg),
                problems,
                resolved,
                last,
            });
        });
    }

    /// Starts a program and doesn't wait (`program` is found on PATH / App Paths first).
    /// One `Ran` saying `what`, or why it couldn't start.
    pub fn run_detached(
        &self,
        what: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
    ) {
        self.blocking_ran(what, move || {
            let exe = crate::open::run::resolve(&program)
                .ok_or_else(|| format!("{program} not found (PATH, App Paths)"))?;
            crate::open::run::spawn_detached(&exe, &args, cwd.as_deref()).map_err(|e| e.to_string())
        });
    }

    /// An app or web pin, or typed text: ShellExecute with `%ENV%` expanded. A browser
    /// (`find`) is located through PATH / App Paths first.
    pub fn launch(&self, what: String, l: crate::launcher::Launch) {
        self.blocking_ran(what, move || {
            let command = if l.find {
                crate::open::run::resolve(&l.command)
                    .ok_or_else(|| format!("{} isn't installed (App Paths)", l.command))?
            } else {
                l.command.clone()
            };
            if cfg!(test) {
                return Ok(());
            }
            crate::platform::shell_execute(&command, &l.args, &l.dir).map_err(|e| e.to_string())
        });
    }

    /// wtm's launcher entries as pins, and its usage re-keyed: one `WtmFound` (`offer`:
    /// the first-run offer rather than `:import-wtm`).
    pub fn wtm_read(&self, offer: bool) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let _ = tx.send(read_wtm(offer));
        });
    }

    /// First run (no pins.toml yet) with a wtm config next to it: offer the import.
    pub fn wtm_offer_check(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let first_run = pins::store_path().is_some_and(|p| !p.exists());
            if first_run && wtm_paths().0.is_some_and(|c| c.is_file()) {
                let _ = tx.send(read_wtm(true));
            }
        });
    }

    /// The Start Menu's apps: one `StartMenu`.
    pub fn scan_start_menu(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let _ = tx.send(AppEvent::StartMenu(crate::launcher::startmenu::scan()));
        });
    }

    /// Month and weekday names from the Windows user locale: one `LocaleNames` (nothing
    /// if Windows won't say; English stays).
    pub fn locale_names(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            if let Some((months, months_short, days, days_short)) = platform::locale_names() {
                let _ = tx.send(AppEvent::LocaleNames(Box::new(
                    crate::rename::tokens::Names {
                        months,
                        months_short,
                        days,
                        days_short,
                    },
                )));
            }
        });
    }

    /// rename.toml and the panel's memory: one `RenameStore`.
    pub fn load_rename_store(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            use crate::rename::presets;
            let presets = presets::presets_path()
                .map(|p| presets::load_presets(&p))
                .unwrap_or(Ok(Vec::new()));
            let state = presets::state_path()
                .map(|p| presets::load_state(&p))
                .unwrap_or_default();
            let _ = tx.send(AppEvent::RenameStore {
                presets,
                state: Box::new(state),
            });
        });
    }

    pub fn save_rename_presets(&self, list: Vec<crate::rename::presets::Preset>) {
        self.rt.spawn_blocking(move || {
            use crate::rename::presets;
            if let Some(p) = presets::presets_path()
                && let Err(e) = presets::save_presets(&p, &list)
            {
                tracing::warn!(error = %e, "saving rename.toml");
            }
        });
    }

    pub fn save_rename_state(&self, state: crate::rename::presets::State) {
        self.rt.spawn_blocking(move || {
            use crate::rename::presets;
            if let Some(p) = presets::state_path()
                && let Err(e) = presets::save_state(&p, &state)
            {
                tracing::warn!(error = %e, "saving rename-state.toml");
            }
        });
    }

    /// Photo / PDF values of the files that have some: `RenameMeta` batches, the last
    /// with `done`. Files are read on a few threads (it's latency on a share).
    pub fn rename_meta(&self, generation: u64, paths: Vec<String>) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let wanted: Vec<String> = paths
                .into_iter()
                .filter(|p| crate::rename::meta::wanted(p))
                .collect();
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..8.min(wanted.len()) {
                    let (tx, wanted, next) = (tx.clone(), &wanted, &next);
                    s.spawn(move || {
                        let mut batch = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(p) = wanted.get(i) else { break };
                            if let Some(m) = crate::rename::meta::read(std::path::Path::new(p)) {
                                batch.push((p.clone(), m));
                            }
                            if batch.len() >= 32 {
                                let _ = tx.send(AppEvent::RenameMeta {
                                    generation,
                                    metas: std::mem::take(&mut batch),
                                    done: false,
                                });
                            }
                        }
                        if !batch.is_empty() {
                            let _ = tx.send(AppEvent::RenameMeta {
                                generation,
                                metas: batch,
                                done: false,
                            });
                        }
                    });
                }
            });
            let _ = tx.send(AppEvent::RenameMeta {
                generation,
                metas: Vec::new(),
                done: true,
            });
        });
    }

    /// The contents of `dirs` (selected folders), all the way down, for the panel's
    /// subfolders toggle: one `RenameWalk`. Links aren't followed; hidden items are left
    /// out unless shown. Stops at 20 000 items.
    pub fn rename_walk(&self, generation: u64, dirs: Vec<String>, hidden: bool) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let (items, existing, capped) = rename_walk(&dirs, hidden, 20_000);
            let _ = tx.send(AppEvent::RenameWalk {
                generation,
                items,
                existing,
                capped,
            });
        });
    }

    /// The `want` newest rename batches (only those under `under`, if given), with which
    /// of their items are still there: one
    /// `RenameJournal`.
    pub fn rename_journal(&self, want: usize, under: Option<String>) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            use crate::rename::apply;
            let batches = apply::journal_dir()
                .map(|d| apply::load(&d))
                .unwrap_or_default()
                .into_iter()
                .filter(|e| under.as_deref().is_none_or(|u| e.batch.touches(u)))
                .take(want)
                .map(|e| {
                    let here = e
                        .batch
                        .items
                        .iter()
                        .map(|it| std::path::Path::new(&it.now).exists())
                        .collect();
                    (e, here)
                })
                .collect();
            let _ = tx.send(AppEvent::RenameJournal {
                want,
                here: under.is_some(),
                batches,
            });
        });
    }

    /// ui.toml (the folder order): one `UiState`.
    pub fn load_ui_state(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let st = crate::state::path()
                .map(|p| crate::state::load(&p))
                .unwrap_or_default();
            let _ = tx.send(AppEvent::UiState(st));
        });
    }

    pub fn save_ui_state(&self, st: crate::state::UiState) {
        self.rt.spawn_blocking(move || {
            if let Some(p) = crate::state::path()
                && let Err(e) = crate::state::save(&p, &st)
            {
                tracing::warn!(error = %e, "saving ui.toml");
            }
        });
    }

    /// The frecency store: one `UsageLoaded`.
    pub fn load_usage(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let usage = crate::frecency::store_path()
                .map(|p| crate::frecency::load(&p))
                .unwrap_or_default();
            let _ = tx.send(AppEvent::UsageLoaded(usage));
        });
    }

    /// Writes the whole frecency store (small: one line per thing ever opened).
    pub fn save_usage(&self, usage: crate::frecency::Usage) {
        self.rt.spawn_blocking(move || {
            if let Some(p) = crate::frecency::store_path()
                && let Err(e) = crate::frecency::save(&p, &usage)
            {
                tracing::warn!(error = %e, "saving usage.tsv");
            }
        });
    }

    /// Explorer with `path` selected.
    pub fn reveal(&self, path: String) {
        let what = format!("revealed {}", short_name(&path));
        self.blocking_ran(what, move || {
            crate::platform::reveal(&path).map_err(|e| e.to_string())
        });
    }

    /// The Properties sheet.
    pub fn properties(&self, path: String) {
        let what = format!("properties of {}", short_name(&path));
        self.blocking_ran(what, move || {
            crate::platform::show_properties(&path).map_err(|e| e.to_string())
        });
    }

    /// Windows' Open with dialog. It's modal, so it gets a thread of its own rather than
    /// holding a pool thread while the user decides.
    pub fn open_with_dialog(&self, path: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = crate::platform::open_with_dialog(&path).map_err(|e| e.to_string());
            // Cancelling the dialog is an error to Windows, not to the user.
            let result = result.or_else(|e| {
                if e.contains("0x800704C7") {
                    Ok(())
                } else {
                    Err(e)
                }
            });
            let _ = tx.send(AppEvent::Ran {
                what: format!("open with: {}", short_name(&path)),
                result,
            });
        });
    }

    /// Paths (UNC-resolved for mapped drives with `unc`) or names to the clipboard, one
    /// per line.
    pub fn copy_text(&self, what: String, lines: Vec<String>, unc: bool) {
        self.blocking_ran(what, move || {
            let lines: Vec<String> = if unc {
                lines.iter().map(|p| crate::open::run::unc_of(p)).collect()
            } else {
                lines
            };
            crate::open::run::copy_text(&lines.join("\r\n"))
        });
    }

    /// What Windows has for `ext` (no dot): its default app's name and the recommended
    /// apps. COM, so on a thread of its own (an STA). One `Registered`.
    pub fn assoc_lookup(&self, ext: String) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let dotted = format!(".{ext}");
            let apps = crate::platform::assoc_handlers(&dotted)
                .unwrap_or_default()
                .into_iter()
                .map(|h| (h.name, h.ui_name))
                .collect();
            let registered = crate::open::Registered {
                default_app: crate::platform::default_app_name(&dotted),
                apps,
            };
            let _ = tx.send(AppEvent::Registered { ext, registered });
        });
    }

    /// Opens `paths` with the app Windows registered as `name` for `ext` (no dot).
    pub fn assoc_invoke(&self, what: String, ext: String, name: String, paths: Vec<String>) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let dotted = format!(".{ext}");
            let result = paths.iter().try_for_each(|p| {
                crate::platform::assoc_invoke(&dotted, &name, p).map_err(|e| e.to_string())
            });
            let _ = tx.send(AppEvent::Ran { what, result });
        });
    }

    pub fn save_open_last(&self, last: crate::open::run::LastUsed) {
        self.rt
            .spawn_blocking(move || crate::open::run::save_last(&last));
    }

    fn blocking_ran(&self, what: String, f: impl FnOnce() -> Result<(), String> + Send + 'static) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let result = f();
            let _ = tx.send(AppEvent::Ran { what, result });
        });
    }

    /// Looks for 7-Zip once: one `SevenZip`.
    pub fn find_7z(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let found = crate::jobs::zip::find_7z().map(|p| p.display().to_string());
            let _ = tx.send(AppEvent::SevenZip(found));
        });
    }

    /// The presets' last runs: one `PresetRuns`.
    pub fn load_preset_runs(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let runs = crate::jobs::preset::runs_path()
                .map(|p| crate::jobs::preset::load_runs(&p))
                .unwrap_or_default();
            let _ = tx.send(AppEvent::PresetRuns(runs));
        });
    }

    /// Records one preset's last run: read, update, write, so a headless `sentier run`
    /// writing the same file in between isn't lost.
    pub fn record_preset_run(&self, key: String, run: crate::jobs::preset::LastRun) {
        self.rt.spawn_blocking(move || {
            let Some(p) = crate::jobs::preset::runs_path() else {
                return;
            };
            let mut runs = crate::jobs::preset::load_runs(&p);
            runs.insert(key, run);
            if let Err(e) = crate::jobs::preset::save_runs(&p, &runs) {
                tracing::warn!(error = %e, "saving presets.json");
            }
        });
    }

    /// Which of `dirs` look like AVEVA projects: every marker glob matches a name at the
    /// folder's top level. A folder that can't be read in time counts as not one.
    pub fn aveva_check(&self, token: u64, dirs: Vec<String>, markers: Vec<String>) {
        let (tx, timeout) = (self.tx.clone(), self.timeouts.network);
        self.rt.spawn(async move {
            let globs: Vec<globset::GlobMatcher> = markers
                .iter()
                .filter_map(|m| {
                    globset::GlobBuilder::new(m)
                        .case_insensitive(true)
                        .build()
                        .ok()
                        .map(|g| g.compile_matcher())
                })
                .collect();
            let mut hits = Vec::new();
            for dir in dirs {
                let d = dir.clone();
                let read = tokio::task::spawn_blocking(move || -> Vec<String> {
                    std::fs::read_dir(&d)
                        .map(|rd| {
                            rd.flatten()
                                .map(|e| e.file_name().to_string_lossy().to_string())
                                .collect()
                        })
                        .unwrap_or_default()
                });
                let names = match tokio::time::timeout(timeout, read).await {
                    Ok(Ok(n)) => n,
                    _ => continue,
                };
                if !globs.is_empty() && globs.iter().all(|g| names.iter().any(|n| g.is_match(n))) {
                    hits.push(dir);
                }
            }
            let _ = tx.send(AppEvent::AvevaChecked { token, hits });
        });
    }

    pub fn save_report(&self, report: crate::jobs::Report) {
        self.rt.spawn_blocking(move || {
            if let Some(dir) = crate::jobs::jobs_dir()
                && let Err(e) = crate::jobs::save_report(&dir, &report)
            {
                tracing::warn!(error = %e, "saving a job report");
            }
        });
    }

    /// Dismissing a job: its report and its log.
    pub fn delete_report(&self, id: u64, log: Option<String>) {
        self.rt.spawn_blocking(move || {
            if let Some(dir) = crate::jobs::jobs_dir() {
                crate::jobs::delete_report(&dir, id, log.as_deref());
            }
        });
    }

    /// The startup clean-up: job logs older than `keep_log_days` (0 keeps them) unless
    /// a kept report points to them, and staging folders left by a killed backup.
    pub fn housekeeping(&self, keep_log_days: u32) {
        self.rt.spawn_blocking(move || {
            let jobs = crate::jobs::jobs_dir();
            let staging = crate::jobs::zip::staging_root();
            let swept = crate::jobs::sweep(
                jobs.as_deref(),
                staging.as_deref(),
                keep_log_days,
                std::time::SystemTime::now(),
            );
            if swept != crate::jobs::Swept::default() {
                tracing::info!(
                    logs = swept.logs,
                    staging = swept.staging,
                    "housekeeping: removed old job logs and staging leftovers"
                );
            }
        });
    }

    /// The reports kept from earlier sessions: one `JobReports`.
    pub fn load_job_reports(&self) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let reports = crate::jobs::jobs_dir()
                .map(|d| crate::jobs::load_reports(&d))
                .unwrap_or_default();
            let _ = tx.send(AppEvent::JobReports(reports));
        });
    }

    /// Runs a file operation on the blocking pool: one `FileOpDone`. With `audit` (an
    /// admin share's machine and user) the change is appended to `audit.log`.
    pub fn file_op(&self, op: crate::fs::ops::FileOp, audit: Option<(String, Option<String>)>) {
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            use crate::fs::ops::FileOp;
            // Windows deletes for good where there's no Recycle Bin: never let a "to the
            // Recycle Bin" through on a share or a mapped drive.
            let result = match &op {
                FileOp::Trash(paths) if paths.iter().any(|p| no_recycle_bin(p)) => {
                    Err("no Recycle Bin on a network drive; D deletes permanently".to_string())
                }
                _ => op.run(),
            };
            if let Some((host, user)) = audit {
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                let line =
                    crate::fs::ops::audit_line(&now, &host, user.as_deref(), &op, result.is_ok());
                append_audit(&line);
            }
            let _ = tx.send(AppEvent::FileOpDone { op, result });
        });
    }

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

/// wtm's `%APPDATA%\wtm\config.toml` and `%LOCALAPPDATA%\wtm\usage.tsv` (none in tests:
/// they never read the user's own files).
fn wtm_paths() -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    if cfg!(test) {
        return (None, None);
    }
    let at = |var: &str, file: &str| {
        std::env::var_os(var).map(|d| std::path::PathBuf::from(d).join("wtm").join(file))
    };
    (
        at("APPDATA", "config.toml"),
        at("LOCALAPPDATA", "usage.tsv"),
    )
}

/// Blocking: reads wtm's config and usage, tells folders from apps.
fn read_wtm(offer: bool) -> AppEvent {
    use crate::launcher::entry;
    let (config, usage) = wtm_paths();
    let result = config
        .ok_or_else(|| "no %APPDATA%".to_string())
        .and_then(|p| std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display())))
        .and_then(|text| entry::parse_config(&text))
        .map(|(entries, problems)| {
            let pairs: Vec<(entry::WtmEntry, crate::pins::Pin)> = entries
                .into_iter()
                .map(|e| {
                    let target = platform::expand_env(&e.command);
                    let is_dir = std::path::Path::new(&target).is_dir();
                    let pin = entry::to_pin(&e, is_dir);
                    (e, pin)
                })
                .collect();
            let wtm_usage = usage.map(|p| crate::frecency::load(&p)).unwrap_or_default();
            (pairs, wtm_usage, problems)
        });
    AppEvent::WtmFound { result, offer }
}

/// `C:\a\b.pdf` → `b.pdf`, for messages.
fn short_name(path: &str) -> String {
    path.trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(path)
        .to_string()
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
    // Machines come with the pins from the app but go last, in their own section.
    let (machines, pins): (Vec<Entry>, Vec<Entry>) = pins
        .into_iter()
        .partition(|e| matches!(e.kind, EntryKind::Machine(_)));
    let mut entries = pins;
    entries.extend(drives);
    entries.extend(saved);
    entries.extend(machines);
    Ok(entries)
}

/// The pins.toml writer: saves in the order asked, skipping to the newest store when
/// several are queued (each one is the whole store).
/// pins.toml's modified time in ms (None: no file), to tell its own saves from others'.
fn pins_mtime() -> Option<u64> {
    let m = std::fs::metadata(pins::store_path()?)
        .ok()?
        .modified()
        .ok()?;
    m.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

async fn pins_writer(
    mut rx: mpsc::UnboundedReceiver<PinStore>,
    tx: EventTx,
    own_write: Arc<AtomicU64>,
) {
    while let Some(mut store) = rx.recv().await {
        while let Ok(newer) = rx.try_recv() {
            store = newer;
        }
        let call = tokio::task::spawn_blocking(move || match pins::store_path() {
            Some(path) => pins::save(&path, &store).map(|()| pins_mtime()),
            None => Err("APPDATA is not set".to_string()),
        });
        match call.await.unwrap_or_else(|e| Err(e.to_string())) {
            // The watcher skips this change: it's ours.
            Ok(m) => own_write.store(m.unwrap_or(0), Ordering::Relaxed),
            Err(e) => {
                let _ = tx.send(AppEvent::PinsSaveFailed(e));
            }
        }
    }
}

/// Every 2 s: did pins.toml change, and not by this sentier (`sentier pin`, an editor,
/// another instance)? Then it's read again: one `PinsChanged`.
async fn pins_watcher(tx: EventTx, own_write: Arc<AtomicU64>) {
    let mut seen: Option<u64> = None;
    let mut first = true;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let probe = tokio::task::spawn_blocking(pins_mtime);
        let Ok(Ok(now)) = tokio::time::timeout(Duration::from_secs(3), probe).await else {
            continue;
        };
        if first {
            (seen, first) = (now, false);
            continue;
        }
        if now == seen {
            continue;
        }
        seen = now;
        if now.is_none() || now == Some(own_write.load(Ordering::Relaxed)) {
            continue;
        }
        let load = tokio::task::spawn_blocking(|| pins::store_path().map(|p| pins::load(&p)));
        if let Ok(Some(Ok((store, _)))) = load.await
            && tx.send(AppEvent::PinsChanged(store)).is_err()
        {
            return;
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

/// The bulk rename's subfolder walk: every item under `dirs`, each folder's contents
/// right after it (sorted as in the view), and the names each folder already holds.
fn rename_walk(
    dirs: &[String],
    hidden: bool,
    limit: usize,
) -> (
    Vec<crate::rename::plan::Source>,
    crate::rename::plan::Existing,
    bool,
) {
    use crate::rename::plan::Source;
    fn walk(
        dir: &str,
        hidden: bool,
        limit: usize,
        items: &mut Vec<Source>,
        existing: &mut crate::rename::plan::Existing,
    ) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut entries: Vec<Entry> = rd.flatten().filter_map(|de| make_entry(&de)).collect();
        sort_entries(&mut entries);
        existing.insert(
            dir.to_lowercase(),
            entries.iter().map(|e| e.name.to_lowercase()).collect(),
        );
        for e in entries {
            if e.is_link || (!hidden && e.is_hidden()) {
                continue;
            }
            if items.len() >= limit {
                return true;
            }
            let is_dir = e.kind == EntryKind::Dir;
            items.push(Source {
                dir: dir.to_string(),
                name: e.name.clone(),
                is_dir,
                created: e.created,
                modified: e.modified,
                ..Source::default()
            });
            if is_dir {
                let sub = format!("{}\\{}", dir.trim_end_matches('\\'), e.name);
                if walk(&sub, hidden, limit, items, existing) {
                    return true;
                }
            }
        }
        false
    }
    let mut items = Vec::new();
    let mut existing = crate::rename::plan::Existing::new();
    let mut capped = false;
    for d in dirs {
        if walk(d, hidden, limit, &mut items, &mut existing) {
            capped = true;
            break;
        }
    }
    (items, existing, capped)
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

    #[test]
    fn source_cmd_output_errors_and_timeout() {
        let limit = Duration::from_secs(10);
        let out = run_source_cmd(r#"echo ["PC-001","PC-002"]"#, limit);
        let Ok(out) = out else { panic!("{out:?}") };
        let hosts: Vec<String> = crate::machines::parse_json(&out)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.host)
            .collect();
        assert_eq!(hosts, ["PC-001", "PC-002"]);

        let err = run_source_cmd("echo nope 1>&2 & exit /b 3", limit);
        assert_eq!(err, Err("exited with 3: nope".to_string()));

        let start = Instant::now();
        let slow = run_source_cmd("ping -n 6 127.0.0.1 >nul", Duration::from_millis(300));
        assert!(slow.is_err_and(|e| e.contains("timed out")));
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "killed, not waited for"
        );
    }
}
