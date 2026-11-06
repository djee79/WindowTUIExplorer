//! Everything the UI loop reacts to arrives as an [`AppEvent`] on one channel.

use crate::fs::Listing;
use crate::fs::path::Loc;
use crate::fs::worker::ListError;
use crate::platform::DiskSpace;
use ratatui::crossterm::event::Event as TermEvent;
use std::sync::Arc;
use std::sync::mpsc;

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
}

pub fn channel() -> (EventTx, EventRx) {
    mpsc::channel()
}
