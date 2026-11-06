//! Probe: the Win32 network calls M2 needs, so the real code isn't written from guesses
//! (CLAUDE.md working agreements).
//!
//! Examples can't import a binary crate, so this repeats the calls standalone. What it
//! answers:
//!
//! 1. `WNetGetConnectionW` — which drive letters are mapped, to what UNC target, and
//!    which error a *local* drive returns (so `locations.rs` can tell them apart).
//! 2. `NetShareEnum` level 1 — struct layout, the `STYPE_*` flags that mark admin shares
//!    (`C$`, `IPC$`) so they can be filtered, and the error on a server we can't query.
//! 3. **How long each call blocks on a dead host.** This decides whether the timeout in
//!    `fs::worker` can rely on the syscall returning, or must abandon a parked
//!    blocking-pool thread (rule 2 and rule 7).
//!
//! ```powershell
//! cargo run --example probe_netapi                      # local machine + mapped drives
//! cargo run --example probe_netapi -- \\srv01           # a real file server
//! cargo run --example probe_netapi -- --dead            # 10.255.255.1, times the stall
//! ```

use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NOT_CONNECTED, WIN32_ERROR};
use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_MASK,
    STYPE_SPECIAL, STYPE_TEMPORARY,
};
use windows::core::{HSTRING, PWSTR};

/// Tells netapi32 to allocate whatever the result needs (MAX_PREFERRED_LENGTH).
const MAX_PREFERRED_LENGTH: u32 = u32::MAX;
/// RFC 5737-ish black hole: routable, never answers. The spec's dead-host test target.
const DEAD_HOST: &str = r"\\10.255.255.1";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dead = args.iter().any(|a| a == "--dead");
    let servers: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();

    println!("== drives: GetLogicalDrives + GetDriveTypeW + WNetGetConnectionW ==\n");
    probe_drives();

    // An empty server name means the local machine: always answers, no network involved.
    println!("\n== NetShareEnum: local machine (servername = NULL) ==\n");
    probe_shares("");

    for s in &servers {
        println!("\n== NetShareEnum: {s} ==\n");
        probe_shares(s);
    }

    if dead {
        println!("\n== dead host: how long do the calls block? ==\n");
        println!("  (nothing is listening; this is the stall the UI must never wait on)");
        probe_shares(DEAD_HOST);
    } else {
        println!("\nskipped the dead-host timing; add --dead to measure it.");
    }
}

fn probe_drives() {
    // SAFETY: no arguments, returns a bitmask of present drive letters.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i);
        let root = HSTRING::from(format!("{letter}:\\"));
        // SAFETY: `root` is a valid NUL-terminated wide string that outlives the call.
        let kind = unsafe { GetDriveTypeW(&root) };
        let (target, elapsed) = timed(|| connection_of(letter));
        let note = match &target {
            Ok(unc) => format!("→ {unc}"),
            Err(e) if *e == ERROR_NOT_CONNECTED => "(local: ERROR_NOT_CONNECTED)".to_string(),
            Err(e) => format!("(error {})", e.0),
        };
        println!("  {letter}:  type {kind}  {note}   [{elapsed:?}]");
    }
}

/// `WNetGetConnectionW` for one drive letter. Sized in two passes: the first call reports
/// the needed length in `ERROR_MORE_DATA`.
fn connection_of(letter: char) -> Result<String, WIN32_ERROR> {
    let local = HSTRING::from(format!("{letter}:"));
    let mut len: u32 = 0;
    // SAFETY: a null output buffer with len 0 asks for the required length only.
    let probe = unsafe { WNetGetConnectionW(&local, None, &mut len) };
    if probe != ERROR_MORE_DATA && probe != WIN32_ERROR(0) {
        return Err(probe);
    }
    // Guard against a driver reporting something absurd, and leave room for the NUL.
    let cap = (len.max(1) as usize).min(32 * 1024) + 1;
    let mut buf = vec![0u16; cap];
    let mut len = cap as u32;
    // SAFETY: `buf` has `len` writable UTF-16 units and outlives the call.
    let rc = unsafe { WNetGetConnectionW(&local, Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    if rc != WIN32_ERROR(0) {
        return Err(rc);
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Ok(String::from_utf16_lossy(&buf[..end]))
}

fn probe_shares(server: &str) {
    let (result, elapsed) = timed(|| shares_of(server));
    match result {
        Ok(shares) => {
            println!("  {} shares in {elapsed:?}", shares.len());
            for s in &shares {
                let kind = match s.kind & STYPE_MASK.0 {
                    0 => "disk",
                    1 => "printq",
                    2 => "device",
                    3 => "ipc",
                    _ => "?",
                };
                let mut tags = Vec::new();
                if s.special {
                    tags.push("SPECIAL");
                }
                if s.temporary {
                    tags.push("TEMPORARY");
                }
                if !s.browsable() {
                    tags.push("← would be hidden by sentier");
                }
                println!(
                    "    {:<24} {kind:<7} 0x{:08x} {:<40} {}",
                    s.name,
                    s.kind,
                    s.remark,
                    tags.join(" ")
                );
            }
        }
        Err(code) => {
            // 5 = access denied, 53 = bad netpath, 2250 = not connected,
            // 1231 = network unreachable, 6118 = browser list unavailable.
            println!("  failed after {elapsed:?}: NetShareEnum returned {code}");
        }
    }
}

struct Share {
    name: String,
    remark: String,
    kind: u32,
    special: bool,
    temporary: bool,
}

impl Share {
    /// What sentier should show: real disk shares, minus the admin/IPC ones.
    fn browsable(&self) -> bool {
        self.kind & STYPE_MASK.0 == STYPE_DISKTREE.0 && !self.special && !self.temporary
    }
}

fn shares_of(server: &str) -> Result<Vec<Share>, u32> {
    let name = HSTRING::from(server);
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total, mut resume) = (0u32, 0u32, 0u32);
    // SAFETY: `name` outlives the call; the out-pointers point at live locals. On success
    // netapi32 allocates `buf`, which we free below.
    let rc = unsafe {
        NetShareEnum(
            &name,
            1,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
            Some(&mut resume),
        )
    };
    // With MAX_PREFERRED_LENGTH, ERROR_MORE_DATA shouldn't happen; accept what we got.
    if rc != 0 && rc != ERROR_MORE_DATA.0 {
        return Err(rc);
    }
    if buf.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: on success netapi32 returned `read` consecutive SHARE_INFO_1 records.
    let items = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
    let shares = items
        .iter()
        .map(|s| Share {
            name: wide(s.shi1_netname),
            remark: wide(s.shi1_remark),
            kind: s.shi1_type.0,
            special: s.shi1_type.0 & STYPE_SPECIAL.0 != 0,
            temporary: s.shi1_type.0 & STYPE_TEMPORARY.0 != 0,
        })
        .collect();
    // SAFETY: `buf` came from netapi32 and isn't used after this.
    let _ = unsafe { NetApiBufferFree(Some(buf as *const _)) };
    if read != total {
        println!("  note: read {read} of {total} entries (resume handle {resume})");
    }
    Ok(shares)
}

fn wide(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: netapi32 hands back NUL-terminated wide strings inside the buffer.
    unsafe { p.to_string() }.unwrap_or_else(|_| "<invalid utf-16>".to_string())
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}
