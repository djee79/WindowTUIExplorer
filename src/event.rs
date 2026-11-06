//! Everything the UI loop reacts to arrives as an [`AppEvent`] on one channel.

use crate::fs::Listing;
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::{ListError, Probe};
use crate::keymap::Keymap;
use crate::pins::PinStore;
use crate::platform::DiskSpace;
use crate::search::content::MatchPreview;
use crate::search::walk::Hit;
use ratatui::crossterm::event::Event as TermEvent;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

pub type EventTx = mpsc::Sender<AppEvent>;
pub type EventRx = mpsc::Receiver<AppEvent>;

#[derive(Debug)]
pub enum AppEvent {
    /// Keyboard, resize, paste: from the input thread.
    Term(TermEvent),
    /// The input thread hit an error and stopped.
    InputClosed(String),
    /// A listing finished (or failed). `id` matches the [`crate::fs::worker::Request`].
    Listed {
        id: u64,
        loc: Loc,
        result: Result<Arc<Listing>, ListError>,
    },
    /// A big listing is still streaming in.
    ListProgress {
        id: u64,
        loc: Loc,
        count: usize,
    },
    DiskSpace {
        loc: Loc,
        space: Option<DiskSpace>,
    },
    Opened {
        loc: Loc,
        result: Result<(), String>,
    },
    /// A Locations reachability probe finished (or timed out).
    Probed {
        loc: Loc,
        result: Result<Probe, ListError>,
    },
    /// The saved shares were read at startup.
    SharesLoaded(Result<Vec<FsPath>, String>),
    /// Writing the saved shares failed.
    SharesSaveFailed(String),
    /// pins.toml was read: the store plus problems to report, or why it couldn't be.
    PinsLoaded(Result<(PinStore, Vec<String>), String>),
    /// Writing pins.toml failed.
    PinsSaveFailed(String),
    /// A picker's matcher has new results.
    Wake,
    /// keymap.toml was read: the built keymap plus any problems to report, or a read error.
    KeymapLoaded(Result<(Keymap, Vec<String>), String>),
    /// A batch of search hits (possibly empty: then it's only progress).
    SearchProgress {
        id: u64,
        hits: Vec<Hit>,
        scanned: u64,
        /// The directory a walker thread is in.
        current: String,
    },
    /// The walk finished or was cancelled.
    SearchDone {
        id: u64,
        scanned: u64,
        errors: u64,
        elapsed: Duration,
        cancelled: bool,
    },
    /// The matching lines of a content-search result, for its preview.
    GrepPreview {
        loc: Loc,
        result: Result<MatchPreview, String>,
    },
}

pub fn channel() -> (EventTx, EventRx) {
    mpsc::channel()
}
