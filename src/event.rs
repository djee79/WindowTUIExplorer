//! Everything the UI loop reacts to arrives as an [`AppEvent`] on one channel.

use crate::fs::Listing;
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::{ListError, Probe};
use crate::keymap::Keymap;
use crate::pins::PinStore;
use crate::platform::DiskSpace;
use crate::preview::ImageKey;
use crate::preview::image::ImagePreview;
use crate::search::content::MatchPreview;
use crate::search::walk::Hit;
use ratatui::crossterm::event::Event as TermEvent;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

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
    /// A file operation finished (rename, new, delete…).
    FileOpDone {
        op: crate::fs::ops::FileOp,
        result: Result<crate::fs::ops::Done, String>,
    },
    /// A job's progress, from the worker (M4).
    Job {
        id: u64,
        ev: crate::jobs::JobEvent,
    },
    /// The job reports kept from earlier sessions.
    JobReports(Vec<crate::jobs::Report>),
    /// A sync preset's dry run (`/MIR /L`): what robocopy would do and its exit code.
    SyncPreviewed {
        token: u64,
        result: Result<(Box<crate::jobs::robocopy::Tracker>, i32), String>,
    },
    /// open.toml read (or the defaults), its problems, every opener's program (None: not
    /// found), and the last opener used per extension.
    OpenConfig {
        cfg: Box<crate::open::rules::OpenConfig>,
        problems: Vec<String>,
        resolved: std::collections::BTreeMap<String, Option<String>>,
        last: crate::open::run::LastUsed,
    },
    /// What Windows has registered for an extension (no dot).
    Registered {
        ext: String,
        registered: crate::open::Registered,
    },
    /// wtm's launcher entries (with the pins they become), its usage, and problems; or
    /// why it couldn't be read. `offer`: the first-run offer.
    WtmFound {
        #[allow(clippy::type_complexity)]
        result: Result<
            (
                Vec<(crate::launcher::entry::WtmEntry, crate::pins::Pin)>,
                crate::frecency::Usage,
                Vec<String>,
            ),
            String,
        >,
        offer: bool,
    },
    /// The Start Menu's apps (name, `.lnk` path).
    StartMenu(crate::launcher::startmenu::Apps),
    /// pins.toml was changed by something else (`sentier pin`, an editor): read again.
    PinsChanged(crate::pins::PinStore),
    /// `:pins restore`: the backups of pins.toml, newest first, each compared with now.
    PinBackups(Vec<crate::pin_backups::Listed>),
    /// A backup is now pins.toml: the store it holds and when it was taken; or why not.
    PinsRestored(Result<(crate::pins::PinStore, String), String>),
    /// Month and weekday names in the Windows user's language (rename tokens).
    LocaleNames(Box<crate::rename::tokens::Names>),
    /// The bulk rename's saved presets (or why rename.toml won't load) and its memory.
    RenameStore {
        presets: Result<Vec<crate::rename::presets::Preset>, String>,
        state: Box<crate::rename::presets::State>,
    },
    /// Photo / PDF values read for the panel opened as `gen` (a batch; more may follow).
    RenameMeta {
        generation: u64,
        metas: Vec<(String, crate::rename::meta::Meta)>,
        done: bool,
    },
    /// The subfolders' contents for the panel `gen`, in order, with what each folder
    /// already holds; `capped`: stopped at the limit.
    RenameWalk {
        generation: u64,
        items: Vec<crate::rename::plan::Source>,
        existing: crate::rename::plan::Existing,
        capped: bool,
    },
    /// The newest journaled rename batches (up to `want`), each item marked present or
    /// gone, for `u` / `:undo-rename`.
    RenameJournal {
        want: usize,
        /// Only batches under the current folder were asked for (`u`).
        here: bool,
        batches: Vec<(crate::rename::apply::Entry, Vec<bool>)>,
    },
    /// ui.toml, read at startup (the folder order).
    UiState(crate::state::UiState),
    /// The frecency store, read at startup.
    UsageLoaded(crate::frecency::Usage),
    /// Something was started, shown or copied (`what`), or why not.
    Ran {
        what: String,
        result: Result<(), String>,
    },
    /// Whether the server of `open` answered on port 445 (a share that won't list).
    SmbAnswer {
        open: String,
        up: bool,
    },
    /// Where 7-Zip is installed (None: it isn't).
    SevenZip(Option<String>),
    /// The presets' last runs, from `presets.json`.
    PresetRuns(crate::jobs::preset::Runs),
    /// The folder an archive extracts into (named by the archive), and whether it's
    /// there already; or why the archive can't be read.
    ArchiveTarget {
        token: u64,
        result: Result<(String, bool), String>,
    },
    /// Which pasted folders look like AVEVA projects.
    AvevaChecked {
        token: u64,
        hits: Vec<String>,
    },
    /// The machine picker's slow sources (inventory CSV, `source_cmd`), each with its
    /// machines or why not.
    MachineSources(
        Vec<(
            &'static str,
            Result<Vec<crate::machines::MachineRecord>, String>,
        )>,
    ),
    /// The port-445 probe of a machine: the round trip, or why not.
    MachineProbe {
        host: String,
        result: Result<Duration, String>,
    },
    /// Who uses a machine (logged on, profile folders).
    MachineUsers {
        host: String,
        result: Result<crate::fs::worker::MachineUsers, String>,
    },
    /// Which template folders exist on a machine.
    PathsChecked {
        host: String,
        found: Vec<(String, bool)>,
    },
    /// The recent machines were read.
    RecentMachines(Result<Vec<crate::machines::Recent>, String>),
    /// config.toml was read (or why not).
    ConfigLoaded(Result<Box<crate::config::Config>, String>),
    /// A connection with credentials finished: `target` is where it went (an alias for
    /// the 1219 fix), the Win32 code on failure, whether the credential was remembered
    /// when asked, and what was disconnected first.
    Connected {
        requested: String,
        target: String,
        user: String,
        result: Result<(), u32>,
        remembered: Option<Result<(), String>>,
        dropped: Vec<String>,
    },
    /// `:net list`: the live connections, or the Win32 code.
    NetList(Result<Vec<crate::platform::NetConnection>, u32>),
    /// `:net drop`: what was disconnected, each with its result.
    NetDropped(Vec<(String, Result<(), u32>)>),
    /// A picture for the preview column, read and encoded (or why not).
    ImagePreview {
        key: ImageKey,
        result: Result<Arc<ImagePreview>, String>,
    },
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
    /// The first lines of a file for the preview column (None: it isn't text).
    TextPreview {
        loc: crate::fs::path::Loc,
        result: Result<Option<Box<crate::preview::text::TextPreview>>, String>,
    },
    GrepPreview {
        loc: Loc,
        result: Result<MatchPreview, String>,
    },
}

pub fn channel() -> (EventTx, EventRx) {
    mpsc::channel()
}

/// Key-to-frame latency (`SENTIER_LOG=latency=debug`): when the oldest key not yet on
/// screen was read, in µs since [`START`] plus one (0: none waiting). The input thread
/// marks, the UI loop takes; nothing ever blocks on it.
static INPUT_AT: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

fn micros_now() -> u64 {
    let since = START.get_or_init(Instant::now).elapsed();
    u64::try_from(since.as_micros()).unwrap_or(u64::MAX - 1) + 1
}

/// The input thread read a key (before it is sent). An older waiting key keeps its time.
pub fn mark_input() {
    let _ = INPUT_AT.compare_exchange(0, micros_now(), Ordering::Relaxed, Ordering::Relaxed);
}

/// How long ago the oldest waiting key was read, if one is waiting; none waits after.
pub fn take_input() -> Option<Duration> {
    match INPUT_AT.swap(0, Ordering::Relaxed) {
        0 => None,
        at => Some(Duration::from_micros(micros_now().saturating_sub(at))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The one test that touches the mark (it is process-wide).
    #[test]
    fn the_oldest_key_keeps_its_time() {
        let _ = take_input();
        assert_eq!(take_input(), None);
        mark_input();
        std::thread::sleep(Duration::from_millis(5));
        mark_input();
        assert!(take_input().is_some_and(|d| d >= Duration::from_millis(5)));
        assert_eq!(take_input(), None, "taken once");
    }
}
