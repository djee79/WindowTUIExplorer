//! Benchmark: directory enumeration over SMB (CLAUDE.md section 12, "Benchmark it").
//!
//! Search on a share is bound by enumeration round trips, not matching. This walks the
//! same tree three ways, at several thread counts, and prints entries/s:
//!
//! - **A** `ignore::WalkParallel`, the walker the spec starts from (uses `read_dir`).
//! - **B** a plain parallel walker over `std::fs::read_dir`.
//! - **C** the same walker over `FindFirstFileExW(FindExInfoBasic,
//!   FIND_FIRST_EX_LARGE_FETCH)`: no 8.3 names, bigger batches per round trip.
//!
//! If C clearly beats A on a share, UNC searches get their own walker; otherwise
//! `WalkParallel` is used everywhere.
//!
//! ```powershell
//! cargo run --release --example enum_bench -- \\server\share\project
//! cargo run --release --example enum_bench -- \\server\share\project --threads 4,16,32
//! cargo run --release --example enum_bench -- C:\Users\me\source       # local baseline
//! ```
//!
//! The SMB client caches directory listings for a few seconds, so every configuration
//! runs **twice** and both times are shown. Compare the second runs, and run the whole
//! benchmark twice in different orders (`--order CBA`) if the numbers look skewed.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Storage::FileSystem::{
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, WIN32_FIND_DATAW,
};
use windows::core::HSTRING;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const SKIP: &[&str] = &["$RECYCLE.BIN", "System Volume Information"];

#[derive(Default)]
struct Totals {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
}

struct Run {
    files: u64,
    dirs: u64,
    bytes: u64,
    errors: u64,
    time: Duration,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(root) = args.iter().find(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: enum_bench <dir> [--threads 1,4,16,32] [--order ABC]");
        std::process::exit(2);
    };
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let threads: Vec<usize> = flag("--threads")
        .unwrap_or_else(|| "1,4,16,32".to_string())
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    let order = flag("--order").unwrap_or_else(|| "ABC".to_string());

    println!("root: {root}");
    println!(
        "cpus: {}",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!();
    println!(
        "{:<28} {:>4}  {:>10} {:>9} {:>12} {:>9} {:>11}  {:>9}",
        "method", "thr", "files", "dirs", "bytes", "errors", "run 1", "run 2"
    );
    for m in order.chars() {
        for &n in &threads {
            let r1 = run(m, &root, n);
            let r2 = run(m, &root, n);
            let (Some(r1), Some(r2)) = (r1, r2) else {
                continue;
            };
            let rate = |r: &Run| (r.files + r.dirs) as f64 / r.time.as_secs_f64().max(1e-9);
            println!(
                "{:<28} {:>4}  {:>10} {:>9} {:>12} {:>9} {:>6.2}s {:>6.0}/s  {:>6.2}s {:>6.0}/s",
                name(m),
                n,
                r2.files,
                r2.dirs,
                r2.bytes,
                r2.errors,
                r1.time.as_secs_f64(),
                rate(&r1),
                r2.time.as_secs_f64(),
                rate(&r2),
            );
        }
    }
    println!();
    println!("Paste this table back. Same file/dir counts across methods = same tree walked.");
}

fn name(m: char) -> &'static str {
    match m {
        'A' => "A ignore::WalkParallel",
        'B' => "B parallel read_dir",
        'C' => "C FindFirstFileExW large",
        _ => "?",
    }
}

fn run(m: char, root: &str, threads: usize) -> Option<Run> {
    let totals = Arc::new(Totals::default());
    let start = Instant::now();
    match m {
        'A' => walk_ignore(root, threads, &totals),
        'B' => walk_queue(root, threads, &totals, list_read_dir),
        'C' => walk_queue(root, threads, &totals, list_find_ex),
        _ => return None,
    }
    let time = start.elapsed();
    Some(Run {
        files: totals.files.load(Ordering::Relaxed),
        dirs: totals.dirs.load(Ordering::Relaxed),
        bytes: totals.bytes.load(Ordering::Relaxed),
        errors: totals.errors.load(Ordering::Relaxed),
        time,
    })
}

// ---------------------------------------------------------------------- A: ignore

fn walk_ignore(root: &str, threads: usize, t: &Arc<Totals>) {
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(false)
        .threads(threads)
        .filter_entry(|e| !SKIP.iter().any(|s| e.file_name().eq_ignore_ascii_case(s)))
        .build_parallel();
    walker.run(|| {
        let t = Arc::clone(t);
        Box::new(move |entry| {
            match entry {
                Ok(e) => match e.metadata() {
                    Ok(m) if m.is_dir() => {
                        t.dirs.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(m) => {
                        t.files.fetch_add(1, Ordering::Relaxed);
                        t.bytes.fetch_add(m.len(), Ordering::Relaxed);
                    }
                    Err(_) => {
                        t.errors.fetch_add(1, Ordering::Relaxed);
                    }
                },
                Err(_) => {
                    t.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            ignore::WalkState::Continue
        })
    });
}

// ------------------------------------------------------- B/C: shared queue walker

/// One directory's entries: (name, is_dir, is_reparse, size).
type Listing = Vec<(String, bool, bool, u64)>;

fn walk_queue(root: &str, threads: usize, t: &Arc<Totals>, list: fn(&str) -> Option<Listing>) {
    struct Q {
        dirs: VecDeque<String>,
        active: usize,
    }
    let q = Arc::new((
        Mutex::new(Q {
            dirs: VecDeque::from([root.trim_end_matches('\\').to_string()]),
            active: 0,
        }),
        Condvar::new(),
    ));
    let workers: Vec<_> = (0..threads.max(1))
        .map(|_| {
            let (q, t) = (Arc::clone(&q), Arc::clone(t));
            std::thread::spawn(move || {
                loop {
                    let dir = {
                        let (lock, cv) = &*q;
                        let Ok(mut g) = lock.lock() else { return };
                        loop {
                            if let Some(d) = g.dirs.pop_front() {
                                g.active += 1;
                                break d;
                            }
                            if g.active == 0 {
                                cv.notify_all();
                                return;
                            }
                            g = match cv.wait(g) {
                                Ok(g) => g,
                                Err(_) => return,
                            };
                        }
                    };
                    let mut subdirs = Vec::new();
                    match list(&dir) {
                        Some(entries) => {
                            for (name, is_dir, reparse, size) in entries {
                                if is_dir {
                                    t.dirs.fetch_add(1, Ordering::Relaxed);
                                    let skip = SKIP.iter().any(|s| name.eq_ignore_ascii_case(s));
                                    if !reparse && !skip {
                                        subdirs.push(format!("{dir}\\{name}"));
                                    }
                                } else {
                                    t.files.fetch_add(1, Ordering::Relaxed);
                                    t.bytes.fetch_add(size, Ordering::Relaxed);
                                }
                            }
                        }
                        None => {
                            t.errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let (lock, cv) = &*q;
                    let Ok(mut g) = lock.lock() else { return };
                    g.dirs.extend(subdirs);
                    g.active -= 1;
                    cv.notify_all();
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

fn list_read_dir(dir: &str) -> Option<Listing> {
    let rd = std::fs::read_dir(PathBuf::from(dir)).ok()?;
    Some(
        rd.filter_map(Result::ok)
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                let name = e.file_name().to_string_lossy().into_owned();
                Some((name, m.is_dir(), m.file_type().is_symlink(), m.len()))
            })
            .collect(),
    )
}

fn list_find_ex(dir: &str) -> Option<Listing> {
    let pattern = HSTRING::from(format!("{dir}\\*"));
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` outlives the call; `data` is a valid WIN32_FIND_DATAW, which is
    // what FindExInfoBasic fills.
    let handle = unsafe {
        FindFirstFileExW(
            &pattern,
            FindExInfoBasic,
            &mut data as *mut WIN32_FIND_DATAW as *mut c_void,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }
    .ok()?;
    let mut out = Vec::new();
    loop {
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf16_lossy(&data.cFileName[..len]);
        if name != "." && name != ".." {
            let attrs = data.dwFileAttributes;
            let size = (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow);
            out.push((
                name,
                attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
                attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                size,
            ));
        }
        // SAFETY: `handle` is a live find handle; `data` is writable.
        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            break;
        }
    }
    // SAFETY: `handle` came from FindFirstFileExW and is closed exactly once.
    let _ = unsafe { FindClose(handle) };
    Some(out)
}
