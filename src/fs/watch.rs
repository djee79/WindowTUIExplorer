//! Folder watching (Yazi-like): a folder on screen re-lists by itself when another
//! program adds, removes, renames or writes something in it.
//!
//! One thread per watched folder (the cwd, and the previewed folder once the cursor
//! rests). The thread blocks in `platform::watch_dir` and sends
//! [`AppEvent::DirChanged`]; the UI re-lists through the normal worker path, so the
//! timeouts and cancellation of rule 2 still apply to the listing itself.

use crate::event::{AppEvent, EventTx};
use crate::fs::path::Loc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Quiet time after a change before re-listing, and the longest a burst can hold a
/// re-list back. A share is re-listed less eagerly: each listing is a round trip.
const LOCAL: (Duration, Duration) = (Duration::from_millis(150), Duration::from_secs(1));
const NETWORK: (Duration, Duration) = (Duration::from_millis(400), Duration::from_secs(3));

/// A running watch; dropping it stops the thread (within a quarter second).
#[derive(Debug)]
pub struct Watch {
    pub loc: Loc,
    stop: Arc<AtomicBool>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts watching `loc` (a real folder; anything else gives `None`).
pub fn watch(loc: Loc, tx: EventTx) -> Option<Watch> {
    let path = loc.as_fs()?.os_string();
    let (settle, max_wait) = if loc.is_network() { NETWORK } else { LOCAL };
    let stop = Arc::new(AtomicBool::new(false));
    if cfg!(test) {
        return Some(Watch { loc, stop });
    }
    let flag = stop.clone();
    let sent = loc.clone();
    let spawned = std::thread::Builder::new()
        .name("watch".into())
        .spawn(move || {
            let r = crate::platform::watch_dir(&path, &flag, settle, max_wait, || {
                let _ = tx.send(AppEvent::DirChanged(sent.clone()));
            });
            if let Err(e) = r {
                // Some NAS boxes refuse change notifications: no live updates there,
                // `R` still refreshes.
                tracing::info!(%path, error = %e, "folder watch ended");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "couldn't start a folder watch");
        return None;
    }
    Some(Watch { loc, stop })
}
