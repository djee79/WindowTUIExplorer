//! App state and the UI event loop. Key dispatch, actions and prompts live in
//! [`keys`].
//!
//! This module never touches the filesystem. It asks the [`Worker`] for things and reacts
//! to the [`AppEvent`]s that come back.

mod archive;
pub mod auth;
mod fileops;
pub(crate) mod finders;
pub use fileops::Suspend;
mod jobs;
pub use jobs::JobRow;
mod backup;
mod help;
mod keys;
pub(crate) mod launcher;
mod machines;
mod open;
pub use open::OpenMenu;
mod pick;
mod picker;
mod pins;
mod pins_restore;
mod places;
mod rename;
pub use rename::{Field as RenameField, RenamePanel};
mod preview;
mod search;
mod sync;
pub use sync::{PreviewStatus, SyncPreviewState};
mod tabs;
mod tags;

pub use keys::{Prompt, PromptKind};
pub use preview::TextState;
pub use search::SearchState;

use crate::event::{AppEvent, EventRx};
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::{ListError, Probe, Request, Worker};
use crate::fs::{Entry, EntryKind, Listing};
use crate::keymap::{Action, Key, Keymap};
use crate::platform::DiskSpace;
use crate::ui;
use ratatui::crossterm::event::{Event, KeyEventKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant, SystemTime};

/// ~60 fps cap.
const FRAME: Duration = Duration::from_millis(16);
/// Spinner frame rate, only while something is in flight.
pub const SPINNER_TICK: Duration = Duration::from_millis(80);
/// While keys stream in (holding `j`), wait this long after the cursor stops before
/// listing the previewed directory, so there isn't a request per row.
const PREVIEW_DELAY: Duration = Duration::from_millis(40);
/// Moves closer together than this are a stream (key repeat is ~33 ms apart). A move
/// after a longer rest is a single step: its preview is asked for at once.
const PREVIEW_BURST: Duration = Duration::from_millis(80);
/// The terminal: stdout behind a buffer, so a frame goes out in as few writes as
/// possible (std's stdout alone flushes every 1 KB).
pub type Term =
    ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::BufWriter<std::io::Stdout>>>;
const STATUS_TTL: Duration = Duration::from_secs(4);
/// Listings kept in memory for instant back/forward navigation.
const MAX_CACHED: usize = 256;
/// When idle, still wake up now and then (status expiry).
const IDLE_WAKE: Duration = Duration::from_millis(500);
/// Locations entries are re-probed this often while the root is on screen.
pub const REPROBE_EVERY: Duration = Duration::from_secs(60);
/// Directory history kept for `H` / `L`.
const MAX_HISTORY: usize = 200;

/// What the last reachability probes of one Locations entry found.
#[derive(Debug, Clone, Default)]
pub struct Reach {
    pub probing: bool,
    /// The most recent finished probe.
    pub last: Option<Result<Probe, ListError>>,
    pub checked_at: Option<SystemTime>,
    /// When it last answered, and how fast.
    pub last_ok: Option<(SystemTime, Duration)>,
}

impl Reach {
    pub fn is_unreachable(&self) -> bool {
        matches!(self.last, Some(Err(_)))
    }
    pub fn is_reachable(&self) -> bool {
        matches!(self.last, Some(Ok(_)))
    }
}

#[derive(Debug, Default)]
pub struct DirState {
    /// Last good listing. Kept while a refresh runs (stale-while-revalidate).
    pub listing: Option<Arc<Listing>>,
    pub loading: Option<Loading>,
    pub error: Option<ListError>,
}

#[derive(Debug, Clone, Copy)]
pub struct Loading {
    pub since: Instant,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// A prompt is open: `:` command line, `/` `?` search, `f` filter.
    Prompt,
    Help,
    /// A picker is open (`<leader>p`); it has its own insert / normal modes.
    Picker,
    /// `zp`: the preview fills the body (zoom, pan, pages, next file).
    Preview,
    /// `:net list`: the live connections.
    NetList,
    /// The credential prompt (a share refused the login, or `:connect`).
    Credentials,
    /// A sync preset's dry-run preview (Enter runs it, `u` mirror/update, `Tab` all).
    SyncPreview,
    /// The bulk rename panel (`r` on a selection, `:rename`).
    Rename,
    /// `O`: the open-with menu.
    OpenWith,
    /// `J`: the jobs panel (list, detail, report).
    Jobs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quit {
    Plain,
    /// `ZZ` / `:wq`: write the cwd to `--cwd-file`.
    WithCwd,
}

/// A yes/no question shown as a popup (Yazi-style): what `ui/dialog.rs` draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogView {
    pub title: String,
    pub message: String,
    /// The files, pins… it touches (the popup shows the first ones).
    pub items: Vec<String>,
    /// It destroys something: drawn red, and Enter means No until changed.
    pub danger: bool,
    /// The button Enter presses.
    pub yes: bool,
    /// The two buttons (`Yes` `No`, `Merge` `New folder`) and the keys that answer.
    pub buttons: [&'static str; 2],
    pub keys: &'static str,
}

/// The last `/` or `?` search, for `n` / `N` and match highlighting.
#[derive(Debug, Clone)]
pub struct Search {
    pub query: String,
    pub backward: bool,
}

impl Search {
    /// Smartcase, as in vim with `ignorecase smartcase`: case-sensitive only when the
    /// query has an uppercase letter.
    pub fn matches(&self, name: &str) -> bool {
        if self.query.is_empty() {
            return false;
        }
        if self.query.chars().any(char::is_uppercase) {
            name.contains(&self.query)
        } else {
            name.to_lowercase().contains(&self.query.to_lowercase())
        }
    }
}

#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub is_error: bool,
    at: Instant,
}

pub struct App {
    worker: Worker,
    pub cwd: Loc,
    pub dirs: HashMap<Loc, DirState>,
    pending: HashMap<Loc, Request>,
    /// Last selected name per directory, so going back lands where you were.
    remembered: HashMap<Loc, String>,

    /// The listing the cwd view was built from, and the visible indices into it
    /// (after the hidden toggle and the `/` filter).
    pub view_listing: Option<Arc<Listing>>,
    pub view: Vec<usize>,
    pub cursor: usize,
    /// First visible row of the cwd column. The UI keeps the cursor in view.
    pub offset: usize,
    /// Height of the cwd column at the last draw (for page moves).
    pub page: usize,

    pub show_hidden: bool,
    /// How folders are ordered (`,m`, `,s`…; kept in ui.toml).
    pub sort: crate::fs::sort::DirSort,
    /// The order changed at this time and isn't in ui.toml yet (saved once keys settle).
    sort_dirty: Option<Instant>,
    /// vim's `number` and `relativenumber`; both on is neovim's hybrid gutter.
    pub number: bool,
    pub relative_number: bool,
    pub filter: String,
    pub mode: Mode,

    pub keymap: Keymap,
    /// Keys of a sequence in progress (`g`, `<leader>`, `d`…), and when it started.
    pub keys_pending: Vec<Key>,
    keys_since: Option<Instant>,
    /// The which-key popup is up (a prefix waited past the timeout).
    which_key: bool,
    /// A count typed before a command (`5j`, `12G`).
    pub count: Option<usize>,
    pub prompt: Option<Prompt>,
    /// The last search; `search_hl` is cleared by `Esc` like `:nohlsearch`.
    pub search: Option<Search>,
    pub search_hl: bool,
    back: Vec<Loc>,
    forward: Vec<Loc>,
    prev_dir: Option<Loc>,
    /// Tabs (`t`, `gt`, `Alt-1…9`): every tab, the active one's slot stale while it's shown
    /// (its state is in the fields above), and which one is active.
    tabs: Vec<tabs::Tab>,
    tab: usize,
    /// Recursive searches (`s`), newest last; each is a results folder.
    searches: Vec<SearchState>,
    next_search_id: u64,
    /// The matches of the content-search result under the cursor (one file at a time).
    grep_preview: Option<search::GrepPreview>,
    /// The first lines of the text file under the cursor.
    pub text_preview: Option<TextState>,

    /// Saved `\\server\share` roots for the Locations root.
    pub shares: Vec<FsPath>,
    /// False until the store has been read; shares browsed before that are merged in.
    shares_loaded: bool,
    /// The pin store (pins.toml), and whether it has been read: nothing is saved before
    /// that, so a slow or failed read never gets overwritten.
    pub pins: crate::pins::PinStore,
    pins_loaded: bool,
    /// A pin being made or edited through prompts.
    pin_draft: Option<pins::PinDraft>,
    /// What `<leader>zc` zips while its name is asked.
    zip_draft: Option<archive::ZipDraft>,
    /// `<leader>zx`: the archive whose target folder is being read, or asked about.
    extract_ask: Option<archive::ExtractAsk>,
    extract_token: u64,
    /// A key that clashes with others (`'dd` while `'d` is D:\): moving those first.
    key_move: Option<pins::KeyMove>,
    /// `'d` when keys start with d (`'dd`, `'dl`): waiting for the second letter.
    pub mark_prefix: Option<char>,
    /// Pick mode: a preset folder chosen by going there (`p` takes it).
    pub pick: Option<pick::Pick>,
    /// The open picker, its pending count and keys (normal mode).
    pub picker: Option<crate::picker::Picker>,
    picker_count: Option<usize>,
    picker_pending: Option<picker::PickerPending>,
    /// The pin picker's query when it last closed, for `:view save`.
    last_picker_query: String,
    /// `:pins restore`: the backups shown in its picker (row ids index this).
    pin_backups: Vec<crate::pin_backups::Listed>,
    /// A y/n question pending (`:tag rename`, deleting a view…), and view / tag edits.
    confirm: Option<tags::Confirm>,
    view_draft: Option<tags::ViewDraft>,
    tag_draft: Option<String>,
    /// Pictures in the preview column: the encoder settings, the picture shown (or on its
    /// way), and recently shown ones.
    graphics: ratatui_image::picker::Picker,
    image: Option<preview::ImageState>,
    image_cache:
        crate::preview::Lru<crate::preview::ImageKey, Arc<crate::preview::image::ImagePreview>>,
    /// Set by the UI each frame: the box a picture may fill, and where one was drawn.
    pub image_area: Option<ratatui::layout::Rect>,
    /// The body of this frame (between the header and the status line), where popups
    /// go. Set by the UI before the columns, so the preview knows whether its picture
    /// stays clear of the one that is up.
    pub body: Option<ratatui::layout::Rect>,
    pub image_drawn: Option<ratatui::layout::Rect>,
    /// An iTerm2 picture for the UI loop to write at `image_drawn` after the frame.
    pub image_place: Option<Arc<crate::preview::image::ImagePreview>>,
    /// The window was resized: the old picture must be erased by hand.
    pub image_resized: bool,
    /// `<C-l>`: clear the terminal and draw it all again on the next frame.
    pub redraw: bool,
    /// When the cursor last moved (a move soon after is part of a stream).
    moved_at: Instant,
    /// From when the picture under the cursor may be read: at once after a single step,
    /// after [`preview::IMAGE_STILL`] while keys stream in.
    image_due: Instant,
    /// The page shown for each PDF previewed (0-based), so coming back keeps the page.
    pdf_pages: HashMap<Loc, u32>,
    /// Pictures Windows has none of / that failed (not retried), and requests on their way.
    image_misses: crate::preview::Lru<crate::preview::ImageKey, String>,
    image_inflight: HashSet<crate::preview::ImageKey>,
    /// Zoom and pan of the full-screen preview (`zp`); reset when the file changes.
    pview: crate::preview::View,
    full_count: Option<usize>,
    full_pending: Option<preview::FullPending>,
    /// config.toml (only `[auth.*]` so far).
    pub config: crate::config::Config,
    /// The credential prompt, servers already asked about (lowercase; once each until a
    /// connection works), and the connections sentier made (`:net drop-all`).
    pub cred: Option<auth::CredPrompt>,
    cred_asked: HashSet<String>,
    /// A share that wouldn't open, waiting for its server's port-445 answer, and the cwd
    /// when asked (the answer is dropped if the user moved on).
    smb_check: Option<(String, Loc)>,
    connections_made: HashSet<String>,
    /// Machines: what's known per host (lowercase), the recent ones (and whether the file
    /// was read), and the host `u` is picking a user for.
    machines: HashMap<String, machines::MachineInfo>,
    pub recent_machines: Vec<crate::machines::Recent>,
    recent_loaded: bool,
    machine_user_host: Option<String>,
    /// The machine picker's slow sources (inventory CSV, `source_cmd`) merged, when they
    /// arrived, whether they're loading, and what went wrong.
    machine_found: Vec<crate::machines::MachineRecord>,
    machine_found_at: Option<Instant>,
    pub machine_found_loading: bool,
    pub machine_found_errors: Vec<String>,
    /// The open machine picker's unpinned rows (the ids of `ItemKind::Machine` items).
    pub machine_rows: Vec<crate::machines::MachineRecord>,
    /// Jobs (M4): the selected names in the cwd, the visual-mode anchor (a row), what
    /// `yy` / `x` put aside, this session's jobs and their cancel tokens, reports kept
    /// from earlier sessions, the panel's cursor, and a paste waiting on the AVEVA check.
    pub marked: HashSet<String>,
    pub visual: Option<usize>,
    pub register: Option<jobs::Register>,
    pub jobs: Vec<crate::jobs::Job>,
    job_cancel: HashMap<u64, crate::jobs::Control>,
    pub old_reports: Vec<crate::jobs::Report>,
    pub jobs_cursor: usize,
    jobs_auto_opened: HashSet<u64>,
    jobs_pending_d: bool,
    paste_token: u64,
    pending_paste: Option<jobs::PendingPaste>,
    /// `p` with names already in the destination, while the popup asks.
    paste_clash: Option<jobs::PasteClash>,
    last_paste: Option<(jobs::Register, bool)>,
    /// Sync presets (M4b): the open dry-run preview and the last runs from presets.json.
    pub sync_preview: Option<SyncPreviewState>,
    pub preset_runs: crate::jobs::preset::Runs,
    /// Where 7-Zip is installed, if it is (found once at startup).
    pub seven_zip: Option<String>,
    /// Open with (M5): open.toml (or the defaults), each opener's program (None: not
    /// found), the last opener per extension, the `O` menu.
    pub open_cfg: crate::open::rules::OpenConfig,
    pub open_resolved: std::collections::BTreeMap<String, Option<String>>,
    pub open_last: crate::open::run::LastUsed,
    pub open_menu: Option<OpenMenu>,
    /// The bulk rename panel (M4c), its options as last used, and month / day names.
    pub rename_panel: Option<RenamePanel>,
    /// The `g?` overlay (its filter and scroll).
    pub help: Option<help::HelpState>,
    pub rename_state: crate::rename::presets::State,
    pub rename_presets: Vec<crate::rename::presets::Preset>,
    /// Bumped each time the panel opens: walk / metadata results for an older one drop.
    pub rename_gen: u64,
    pub rename_names: crate::rename::tokens::Names,
    /// What Windows has per extension (asked once per session).
    pub open_registered: HashMap<String, crate::open::Registered>,
    /// Frecency of everything opened (launcher ranking), from usage.tsv.
    pub usage: crate::frecency::Usage,
    /// Visits recorded since usage.tsv was last written, and when that was.
    usage_dirty: bool,
    /// pins.toml changed outside, waiting for an open pin edit to end.
    pins_pending: Option<crate::pins::PinStore>,
    usage_saved: Instant,
    /// The Start Menu's apps once scanned, and whether the launcher lists them (`^s`).
    pub start_menu: Option<crate::launcher::startmenu::Apps>,
    /// The finder pickers (M6): the walk feeding the picker, its rows' paths, grep's
    /// root and next walk, and the commands picker's actions.
    picker_walk: Option<finders::PickerWalk>,
    pub picker_paths: Vec<finders::PickedPath>,
    grep_root: Option<crate::fs::path::FsPath>,
    grep_due: Option<Instant>,
    picker_actions: Vec<Action>,
    pub launcher_start_menu: bool,
    sync_token: u64,
    /// File operations: the open rename / delete prompt's subject, how many are running,
    /// the name to put the cursor on when the listing comes back, and a program to run
    /// with the TUI suspended (`:rename` in the editor).
    op_draft: Option<fileops::OpDraft>,
    ops_running: usize,
    select_next: Option<String>,
    pub inline: Option<fileops::Inline>,
    last_job_id: u64,
    /// `:net drop <server>` waiting for the connection list; `:net list` shown.
    net_drop_server: Option<String>,
    pub net_listing: Option<auth::NetListing>,
    /// Reachability of Locations entries, keyed by the location each one opens.
    pub reach: HashMap<Loc, Reach>,
    last_probe_round: Option<Instant>,

    pub status: Option<Status>,
    /// Free space of the cwd's volume: (volume root, space).
    pub space: Option<(Loc, DiskSpace)>,
    space_for: Option<Loc>,

    preview_due: Option<Instant>,
    pub dirty: bool,
    pub quit: Option<Quit>,
    pub started: Instant,
    last_spin_frame: usize,
}

impl App {
    pub fn new(worker: Worker, start: Loc) -> Self {
        let mut app = App {
            worker,
            cwd: Loc::Root,
            dirs: HashMap::new(),
            pending: HashMap::new(),
            remembered: HashMap::new(),
            view_listing: None,
            view: Vec::new(),
            cursor: 0,
            offset: 0,
            page: 20,
            show_hidden: false,
            sort: Default::default(),
            sort_dirty: None,
            number: true,
            relative_number: true,
            filter: String::new(),
            mode: Mode::Normal,
            keymap: Keymap::default(),
            keys_pending: Vec::new(),
            keys_since: None,
            which_key: false,
            count: None,
            prompt: None,
            search: None,
            search_hl: false,
            back: Vec::new(),
            forward: Vec::new(),
            prev_dir: None,
            tabs: vec![tabs::Tab::new(Loc::Root)],
            tab: 0,
            searches: Vec::new(),
            next_search_id: 1,
            grep_preview: None,
            text_preview: None,
            shares: Vec::new(),
            shares_loaded: false,
            pins: crate::pins::PinStore::default(),
            pins_loaded: false,
            pin_draft: None,
            zip_draft: None,
            extract_ask: None,
            extract_token: 0,
            key_move: None,
            mark_prefix: None,
            pick: None,
            picker: None,
            picker_count: None,
            picker_pending: None,
            last_picker_query: String::new(),
            pin_backups: Vec::new(),
            confirm: None,
            view_draft: None,
            tag_draft: None,
            graphics: crate::preview::graphics(),
            image: None,
            image_cache: crate::preview::Lru::new(crate::preview::CACHE_ENTRIES),
            image_area: None,
            body: None,
            image_drawn: None,
            image_place: None,
            image_resized: false,
            redraw: false,
            moved_at: Instant::now(),
            image_due: Instant::now(),
            pdf_pages: HashMap::new(),
            image_misses: crate::preview::Lru::new(256),
            image_inflight: HashSet::new(),
            pview: crate::preview::View::default(),
            full_count: None,
            full_pending: None,
            config: crate::config::Config::default(),
            cred: None,
            cred_asked: HashSet::new(),
            smb_check: None,
            connections_made: HashSet::new(),
            machines: HashMap::new(),
            recent_machines: Vec::new(),
            recent_loaded: false,
            machine_user_host: None,
            machine_found: Vec::new(),
            machine_found_at: None,
            machine_found_loading: false,
            machine_found_errors: Vec::new(),
            machine_rows: Vec::new(),
            marked: HashSet::new(),
            visual: None,
            register: None,
            jobs: Vec::new(),
            job_cancel: HashMap::new(),
            old_reports: Vec::new(),
            jobs_cursor: 0,
            jobs_auto_opened: HashSet::new(),
            jobs_pending_d: false,
            paste_token: 0,
            pending_paste: None,
            paste_clash: None,
            last_paste: None,
            sync_preview: None,
            preset_runs: Default::default(),
            seven_zip: None,
            open_cfg: Default::default(),
            open_resolved: Default::default(),
            open_last: Default::default(),
            open_menu: None,
            rename_panel: None,
            help: None,
            rename_state: Default::default(),
            rename_presets: Vec::new(),
            rename_gen: 0,
            rename_names: Default::default(),
            open_registered: HashMap::new(),
            usage: Default::default(),
            usage_dirty: false,
            pins_pending: None,
            usage_saved: Instant::now(),
            start_menu: None,
            picker_walk: None,
            picker_paths: Vec::new(),
            grep_root: None,
            grep_due: None,
            picker_actions: Vec::new(),
            launcher_start_menu: false,
            sync_token: 0,
            op_draft: None,
            ops_running: 0,
            select_next: None,
            inline: None,
            last_job_id: 0,
            net_drop_server: None,
            net_listing: None,
            reach: HashMap::new(),
            last_probe_round: None,
            status: None,
            space: None,
            space_for: None,
            preview_due: None,
            dirty: true,
            quit: None,
            started: Instant::now(),
            last_spin_frame: 0,
        };
        app.worker.load_shares();
        app.worker.load_pins();
        app.worker.load_config();
        app.worker
            .load_recent_machines(crate::machines::DEFAULT_RECENT_DAYS);
        app.worker.load_keymap();
        app.worker.load_job_reports();
        app.worker.load_preset_runs();
        app.worker.find_7z();
        app.worker.load_open_config();
        app.worker.load_usage();
        app.worker.locale_names();
        app.worker.load_rename_store();
        app.worker.load_ui_state();
        app.worker.wtm_offer_check();
        app.set_cwd(start, None);
        app
    }

    /// True once a pending key sequence has waited long enough to show which-key.
    pub fn which_key_visible(&self) -> bool {
        self.which_key && (!self.keys_pending.is_empty() || self.mark_prefix.is_some())
    }

    pub fn at_root(&self) -> bool {
        self.cwd == Loc::Root
    }

    /// The location a Locations root entry opens (`C:` → `C:\`, `\\srv\s` → itself).
    pub fn root_target(entry: &Entry) -> Option<Loc> {
        Loc::Root.join(&entry.name).ok()
    }

    pub fn reach_of(&self, loc: &Loc) -> Option<&Reach> {
        self.reach.get(loc)
    }

    // ---------------------------------------------------------------- queries

    pub fn selected(&self) -> Option<&Entry> {
        let listing = self.view_listing.as_ref()?;
        listing.entries.get(*self.view.get(self.cursor)?)
    }

    /// The location shown in the preview column, if the selection is a container.
    pub fn preview_loc(&self) -> Option<Loc> {
        if self.mode == Mode::Picker {
            return self.picker_preview_loc();
        }
        let e = self.selected()?;
        if !e.is_container() {
            return None;
        }
        self.cwd.join(&e.name).ok()
    }

    pub fn remembered(&self, loc: &Loc) -> Option<&str> {
        self.remembered.get(loc).map(String::as_str)
    }

    pub fn spinner_frame(&self, now: Instant) -> usize {
        (now.duration_since(self.started).as_millis() / SPINNER_TICK.as_millis()) as usize
    }

    pub fn is_busy(&self) -> bool {
        !self.pending.is_empty() || self.searching() || self.image_busy() || self.ops_running > 0
    }

    pub fn dir(&self, loc: &Loc) -> Option<&DirState> {
        self.dirs.get(loc)
    }

    // ------------------------------------------------------------- navigation

    /// A user navigation: records history (`H`/`L`) and the previous dir (`''`).
    fn navigate(&mut self, loc: Loc, select: Option<String>) {
        if loc != self.cwd {
            self.back.push(self.cwd.clone());
            if self.back.len() > MAX_HISTORY {
                self.back.remove(0);
            }
            self.forward.clear();
        }
        self.set_cwd(loc, select);
    }

    fn set_cwd(&mut self, loc: Loc, select: Option<String>) {
        self.remember_cursor();
        if loc != self.cwd {
            // Every real folder visited counts for `<leader>fr` (saved in `tick`).
            if let Loc::Fs(p) = &loc {
                self.usage
                    .record_visit(&p.display(), crate::frecency::now_secs());
                self.usage_dirty = true;
            }
            self.prev_dir = Some(std::mem::replace(&mut self.cwd, loc));
        }
        self.filter.clear();
        // A live filter belongs to the directory it was typed in.
        if self
            .prompt
            .as_ref()
            .is_some_and(|p| p.kind == PromptKind::Filter)
        {
            self.prompt = None;
            self.mode = Mode::Normal;
        }
        if let Some(name) = select {
            self.remembered.insert(self.cwd.clone(), name);
        }
        self.view_listing = None;
        self.view.clear();
        self.cursor = 0;
        self.marked.clear();
        self.visual = None;
        self.select_next = None;
        self.offset = 0;

        // A machine view, or a path on a machine's admin share: remember the machine and
        // probe port 445 before SMB gets a chance to hang on it.
        if let Some(host) = self.cwd.admin_host().map(str::to_string) {
            self.touch_machine(&host);
        }
        // Show the cached listing right away, refresh it in the background.
        self.request(self.cwd.clone(), true);
        if let Some(parent) = self.cwd.parent() {
            self.request(parent, false);
        }
        self.rebuild_view();
        self.request_space(false);
        self.schedule_preview();
        self.cancel_unneeded();
        if self.at_root() {
            self.probe_root(true);
        }
        self.dirty = true;
    }

    fn remember_cursor(&mut self) {
        if let Some(name) = self.selected().map(|e| e.name.clone()) {
            self.remembered.insert(self.cwd.clone(), name);
        }
    }

    fn go_parent(&mut self) {
        // The root of `\\HOST\C$` of a known machine goes back to its machine view.
        if let Some(view) = self.machine_parent() {
            return self.navigate(view, None);
        }
        if let Some(parent) = self.cwd.parent() {
            let name = self.cwd.name();
            self.navigate(parent, Some(name));
        }
    }

    /// Enter on something known to be unreachable: say so, probe again for next time, and
    /// on a share ask whether its server answers (if it does, the login screen opens).
    pub(super) fn stay_put(&mut self, name: &str, target: Loc, e: &ListError) {
        let share = crate::auth::connect_target(&target);
        match share {
            Some(t)
                if !self
                    .cred_asked
                    .contains(&crate::auth::server_of(&t).to_lowercase()) =>
            {
                let server = crate::auth::server_of(&t).to_string();
                self.set_status(
                    format!("{name} is {e}; checking whether {server} answers…"),
                    true,
                );
                self.check_smb(target.display());
            }
            Some(_) => self.set_status(
                format!("{name} is {e}; probing again… (:connect logs in)"),
                true,
            ),
            None => self.set_status(format!("{name} is {e}; probing again…"), true),
        }
        self.probe(target);
    }

    fn open_selected(&mut self) {
        let Some(entry) = self.selected() else { return };
        let (name, container) = (entry.name.clone(), entry.is_container());
        if self.template_needs_user(&name) {
            return;
        }
        // A Locations entry known to be unreachable: say so and stay put (never hang),
        // and probe again so a second Enter works once it's back.
        if self.at_root()
            && let Ok(target) = self.cwd.join(&name)
            && let Some(Err(e)) = self.reach.get(&target).and_then(|r| r.last.clone())
        {
            return self.stay_put(&name, target, &e);
        }
        match self.cwd.join(&name) {
            Ok(target) if container => self.navigate(target, None),
            // A file: its rule's first opener (open.toml; by default the Windows app),
            // on the selection when there is one.
            Ok(target) if target.as_fs().is_some() => {
                let mut targets = self.open_targets();
                targets.retain(|t| !t.is_dir);
                self.open_with_first(targets);
            }
            Ok(target) => {
                self.set_status(format!("opening {name}…"), false);
                self.worker.open(target);
            }
            Err(e) => self.set_status(e.to_string(), true),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() - 1;
        let target = self.cursor.saturating_add_signed(delta).min(last);
        self.set_cursor(target);
    }

    fn set_cursor(&mut self, idx: usize) {
        let idx = idx.min(self.view.len().saturating_sub(1));
        if idx != self.cursor {
            self.cursor = idx;
            self.schedule_preview();
        }
        self.dirty = true;
    }

    // --------------------------------------------------------------- requests

    /// Lists `loc` unless a request is already running. Without `force`, a cached
    /// listing or error is good enough.
    fn request(&mut self, loc: Loc, force: bool) {
        // Search results are the app's own state; there's nothing to list.
        if self.pending.contains_key(&loc) || matches!(loc, Loc::Results { .. }) {
            return;
        }
        // Machine views are built from what's known (templates, probes, users).
        if matches!(loc, Loc::Machines | Loc::Machine(_)) {
            self.build_machine_view(&loc);
            return;
        }
        let state = self.dirs.entry(loc.clone()).or_default();
        if !force && (state.listing.is_some() || state.error.is_some()) {
            return;
        }
        state.loading = Some(Loading {
            since: Instant::now(),
            count: 0,
        });
        let req = match loc {
            Loc::Root => {
                let mut extra = self.pin_entries();
                extra.extend(self.machine_root_entries());
                self.worker.list_root(self.shares.clone(), extra)
            }
            _ => self.worker.list(loc.clone()),
        };
        self.pending.insert(loc, req);
    }

    /// Re-lists the Locations root after the saved shares changed, if it's cached.
    /// An in-flight listing carries the old share list, so it's replaced.
    fn reload_root(&mut self) {
        if let Some(req) = self.pending.remove(&Loc::Root) {
            req.cancel.cancel();
        }
        if self.dirs.contains_key(&Loc::Root) {
            self.request(Loc::Root, true);
        }
    }

    // ------------------------------------------------------------ locations

    fn probe(&mut self, loc: Loc) {
        let r = self.reach.entry(loc.clone()).or_default();
        if r.probing {
            return;
        }
        r.probing = true;
        self.worker.probe(loc);
        self.dirty = true;
    }

    /// Probes every Locations entry. Without `all`, only those never probed.
    fn probe_root(&mut self, all: bool) {
        let Some(listing) = self.dirs.get(&Loc::Root).and_then(|s| s.listing.clone()) else {
            return;
        };
        if all {
            self.last_probe_round = Some(Instant::now());
        }
        for target in listing.entries.iter().filter_map(App::root_target) {
            // Machines get the port-445 probe (their dot comes from it).
            if let Loc::Machine(host) = &target {
                self.probe_machine_quietly(host, all);
                continue;
            }
            if all || !self.reach.contains_key(&target) {
                self.probe(target);
            }
        }
    }

    /// A listing under a share succeeded: save that share for the Locations root.
    fn remember_share(&mut self, loc: &Loc) {
        let Some(root) = loc
            .as_fs()
            .filter(|p| p.is_network())
            .map(FsPath::volume_root)
        else {
            return;
        };
        if self.shares.contains(&root) {
            return;
        }
        tracing::info!(share = %root.display(), "saving share");
        self.shares.push(root);
        // Before the store is read, saving would overwrite it; the merge saves later.
        if self.shares_loaded {
            self.worker.save_shares(self.shares.clone());
        }
        self.reload_root();
    }

    fn on_shares_loaded(&mut self, result: Result<Vec<FsPath>, String>) {
        let mut loaded = match result {
            Ok(list) => list,
            Err(e) => {
                // Keep running with what we have, and don't save over a file we
                // couldn't read.
                tracing::warn!(error = %e, "couldn't read saved shares");
                self.set_status(format!("couldn't read saved shares: {e}"), true);
                return;
            }
        };
        let extra: Vec<FsPath> = std::mem::take(&mut self.shares)
            .into_iter()
            .filter(|p| !loaded.contains(p))
            .collect();
        let changed = !extra.is_empty();
        loaded.extend(extra);
        self.shares = loaded;
        self.shares_loaded = true;
        if changed {
            self.worker.save_shares(self.shares.clone());
        }
        self.reload_root();
    }

    /// `dd` in Locations: forget the saved share under the cursor.
    fn forget_selected_share(&mut self) {
        let Some(entry) = self.selected().filter(|e| e.kind == EntryKind::Share) else {
            self.set_status(
                "dd removes a saved share; drives can't be removed".into(),
                true,
            );
            return;
        };
        let name = entry.name.clone();
        let Some(Loc::Fs(target)) = App::root_target(entry) else {
            return;
        };
        self.shares.retain(|p| *p != target);
        self.reach.remove(&Loc::Fs(target));
        if self.shares_loaded {
            self.worker.save_shares(self.shares.clone());
        }
        self.reload_root();
        self.set_status(
            format!("removed {name} (browse it again to bring it back)"),
            false,
        );
    }

    fn on_probed(&mut self, loc: Loc, result: Result<Probe, ListError>) {
        let r = self.reach.entry(loc.clone()).or_default();
        r.probing = false;
        r.checked_at = Some(SystemTime::now());
        match &result {
            Ok(p) => r.last_ok = Some((SystemTime::now(), p.rtt)),
            Err(e) => tracing::info!(%loc, error = %e, "probe failed"),
        }
        r.last = Some(result);
        self.dirty = true;
    }

    /// Navigating away cancels in-flight requests for directories no longer on screen.
    fn cancel_unneeded(&mut self) {
        let mut keep: HashSet<Loc> = HashSet::from([self.cwd.clone()]);
        keep.extend(self.cwd.parent());
        keep.extend(self.preview_loc());
        keep.extend(self.completion_dir());
        let stale: Vec<Loc> = self
            .pending
            .keys()
            .filter(|l| !keep.contains(*l))
            .cloned()
            .collect();
        for loc in stale {
            if let Some(req) = self.pending.remove(&loc) {
                req.cancel.cancel();
            }
            if let Some(state) = self.dirs.get_mut(&loc) {
                state.loading = None;
            }
        }
    }

    fn evict_cache(&mut self) {
        if self.dirs.len() <= MAX_CACHED {
            return;
        }
        let mut keep: HashSet<Loc> = HashSet::from([self.cwd.clone()]);
        keep.extend(self.cwd.parent());
        keep.extend(self.preview_loc());
        let mut old: Vec<(Instant, Loc)> = self
            .dirs
            .iter()
            .filter(|(l, s)| !keep.contains(*l) && s.loading.is_none())
            .map(|(l, s)| {
                (
                    s.listing.as_ref().map_or(self.started, |x| x.loaded_at),
                    l.clone(),
                )
            })
            .collect();
        old.sort_by_key(|(t, _)| *t);
        let excess = self.dirs.len() - MAX_CACHED;
        for (_, loc) in old.into_iter().take(excess) {
            self.dirs.remove(&loc);
        }
    }

    fn request_space(&mut self, force: bool) {
        let Some(volume) = self.cwd.real().as_fs().map(|p| Loc::Fs(p.volume_root())) else {
            // Virtual locations have no volume: don't keep showing the last one's space.
            self.space = None;
            self.space_for = None;
            return;
        };
        if force || self.space_for.as_ref() != Some(&volume) {
            self.space_for = Some(volume.clone());
            self.worker.disk_space(volume);
        }
    }

    fn schedule_preview(&mut self) {
        let now = Instant::now();
        // A single step shows its preview at once; only a stream of moves waits for
        // the cursor to stop.
        let stream = now.duration_since(self.moved_at) < PREVIEW_BURST;
        self.preview_due = Some(if stream { now + PREVIEW_DELAY } else { now });
        self.image_due = if stream {
            now + preview::IMAGE_STILL
        } else {
            now
        };
        self.moved_at = now;
        // Another file: zoom and pan start over.
        self.pview = crate::preview::View::default();
    }

    fn refresh(&mut self) {
        // `R` in a machine view probes the machine and looks up its users again; in
        // the Machines list, every listed machine.
        match self.cwd.clone() {
            Loc::Machine(host) => self.reprobe_machine(&host),
            Loc::Machines => {
                let hosts: Vec<String> = self
                    .view_listing
                    .as_ref()
                    .map(|l| l.entries.iter().map(|e| e.name.clone()).collect())
                    .unwrap_or_default();
                for h in hosts {
                    self.reprobe_machine(&h);
                }
            }
            _ => {}
        }
        self.request(self.cwd.clone(), true);
        if let Some(p) = self.preview_loc() {
            self.request(p, true);
        }
        self.request_space(true);
        if self.at_root() {
            self.probe_root(true);
            self.set_status("re-probing every location…".to_string(), false);
        } else {
            self.set_status("refreshing…".to_string(), false);
        }
    }

    // ------------------------------------------------------------------ views

    /// Rebuilds the visible cwd rows from its listing, keeping the selection by name.
    fn rebuild_view(&mut self) {
        let keep = self
            .select_next
            .clone()
            .or_else(|| self.selected().map(|e| e.name.clone()))
            .or_else(|| self.remembered.get(&self.cwd).cloned());
        let in_results = matches!(self.cwd, Loc::Results { .. });
        let listing = if in_results {
            self.results_listing(&self.cwd)
        } else {
            self.dirs.get(&self.cwd).and_then(|s| s.listing.clone())
        };
        // Results already honour the query's `hidden:`; don't hide them again.
        let show_hidden = self.show_hidden || in_results;
        self.view = match &listing {
            // The Locations filter speaks the pin query: `#aveva is:unc on:srv01 hit`.
            Some(l) if self.at_root() => self.root_visible(l),
            Some(l) => visible_indices(l, show_hidden, &self.filter),
            None => Vec::new(),
        };
        // The folder order (results come ordered by their own sort).
        if !in_results
            && !self.at_root()
            && let Some(l) = &listing
        {
            self.sort.apply(&l.entries, &mut self.view);
        }
        self.view_listing = listing;
        let found = keep.and_then(|name| {
            let entries = &self.view_listing.as_ref()?.entries;
            self.view
                .iter()
                .position(|&i| entries[i].name.eq_ignore_ascii_case(&name))
        });
        if found.is_some() {
            self.select_next = None;
        }
        let cursor = found.unwrap_or(self.cursor.min(self.view.len().saturating_sub(1)));
        if cursor != self.cursor || found.is_none() {
            self.schedule_preview();
        }
        self.cursor = cursor;
        self.dirty = true;
    }

    // ----------------------------------------------------------------- status

    fn set_status(&mut self, text: String, is_error: bool) {
        self.status = Some(Status {
            text,
            is_error,
            at: Instant::now(),
        });
        self.dirty = true;
    }

    // ----------------------------------------------------------------- events

    pub fn handle(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Term(Event::Key(key)) => {
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    self.on_key(key);
                }
            }
            AppEvent::Term(Event::Resize(..)) => {
                self.dirty = true;
                // ratatui redraws everything, but an iTerm2 picture survives that.
                self.image_resized = true;
            }
            AppEvent::ImagePreview { key, result } => self.on_image_preview(key, result),
            AppEvent::Term(_) => {}
            AppEvent::InputClosed(e) => {
                tracing::error!(error = %e, "input thread stopped");
                self.quit = Some(Quit::Plain);
            }
            AppEvent::Listed { id, loc, result } => self.on_listed(id, loc, result),
            AppEvent::ListProgress { id, loc, count } => {
                if self.pending.get(&loc).is_some_and(|r| r.id == id) {
                    if let Some(l) = self.dirs.get_mut(&loc).and_then(|s| s.loading.as_mut()) {
                        l.count = count;
                    }
                    self.dirty = true;
                }
            }
            AppEvent::DiskSpace { loc, space } => {
                if self.space_for.as_ref() == Some(&loc) {
                    self.space = space.map(|s| (loc, s));
                    self.dirty = true;
                }
            }
            AppEvent::Opened { loc, result } => match result {
                Ok(()) => self.set_status(format!("opened {}", loc.name()), false),
                Err(e) => self.set_status(format!("can't open {}: {e}", loc.name()), true),
            },
            AppEvent::Probed { loc, result } => self.on_probed(loc, result),
            AppEvent::SharesLoaded(result) => self.on_shares_loaded(result),
            AppEvent::SharesSaveFailed(e) => {
                tracing::warn!(error = %e, "couldn't save shares");
                self.set_status(format!("couldn't save the shares list: {e}"), true);
            }
            AppEvent::Wake => {}
            AppEvent::ConfigLoaded(result) => match result {
                Ok(config) => {
                    self.config = *config;
                    self.apply_recent_days();
                    self.apply_config();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "config.toml");
                    self.set_status(e, true);
                }
            },
            AppEvent::Connected {
                requested,
                target,
                user,
                result,
                remembered,
                dropped,
            } => self.on_connected(requested, target, user, result, remembered, dropped),
            AppEvent::NetList(result) => self.on_net_list(result),
            AppEvent::MachineProbe { host, result } => self.on_machine_probe(host, result),
            AppEvent::MachineUsers { host, result } => self.on_machine_users(host, result),
            AppEvent::PathsChecked { host, found } => self.on_paths_checked(host, found),
            AppEvent::RecentMachines(result) => self.on_recent_machines(result),
            AppEvent::MachineSources(results) => self.on_machine_sources(results),
            AppEvent::Job { id, ev } => self.on_job_event(id, ev),
            AppEvent::FileOpDone { op, result } => self.on_file_op_done(op, result),
            AppEvent::JobReports(reports) => self.on_job_reports(reports),
            AppEvent::AvevaChecked { token, hits } => self.on_aveva_checked(token, hits),
            AppEvent::ArchiveTarget { token, result } => self.on_archive_target(token, result),
            AppEvent::SyncPreviewed { token, result } => self.on_sync_previewed(token, result),
            AppEvent::SevenZip(path) => self.seven_zip = path,
            AppEvent::OpenConfig {
                cfg,
                problems,
                resolved,
                last,
            } => self.on_open_config(*cfg, problems, resolved, last),
            AppEvent::Ran { what, result } => self.on_ran(what, result),
            // Launches recorded before the file was read stay.
            AppEvent::UsageLoaded(usage) => self.usage.merge(usage),
            // The saved order, unless it was already changed in this session.
            AppEvent::UiState(st) if self.sort_dirty.is_none() => {
                self.sort = st.sort();
                self.rebuild_view();
            }
            AppEvent::UiState(_) => {}
            AppEvent::LocaleNames(names) => self.on_locale_names(*names),
            AppEvent::RenameStore { presets, state } => self.on_rename_store(presets, *state),
            AppEvent::RenameMeta {
                generation,
                metas,
                done,
            } => self.on_rename_meta(generation, metas, done),
            AppEvent::RenameWalk {
                generation,
                items,
                existing,
                capped,
            } => self.on_rename_walk(generation, items, existing, capped),
            AppEvent::RenameJournal {
                want,
                here,
                batches,
            } => self.on_rename_journal(want, here, batches),
            AppEvent::PinsChanged(store) => {
                self.pins_pending = Some(store);
                self.apply_pending_pins();
            }
            AppEvent::PinBackups(found) => self.on_pin_backups(found),
            AppEvent::PinsRestored(result) => self.on_pins_restored(result),
            AppEvent::WtmFound { result, offer } => self.on_wtm_found(result, offer),
            AppEvent::StartMenu(apps) => self.on_start_menu(apps),
            AppEvent::Registered { ext, registered } => self.on_registered(ext, registered),
            AppEvent::SmbAnswer { open, up } => self.on_smb_answer(open, up),
            AppEvent::PresetRuns(runs) => {
                // A run recorded before the file was read is newer: keep it.
                for (k, v) in runs {
                    self.preset_runs.entry(k).or_insert(v);
                }
            }
            AppEvent::NetDropped(results) => self.on_net_dropped(results),
            AppEvent::PinsLoaded(result) => self.on_pins_loaded(result),
            AppEvent::PinsSaveFailed(e) => {
                tracing::warn!(error = %e, "couldn't save pins");
                self.set_status(format!("couldn't save pins: {e}"), true);
            }
            AppEvent::KeymapLoaded(result) => self.on_keymap_loaded(result),
            AppEvent::SearchProgress {
                id,
                hits,
                scanned,
                current,
            } => {
                if !self.finders_progress(id, &hits, scanned) {
                    self.on_search_progress(id, hits, scanned, current);
                }
            }
            AppEvent::SearchDone {
                id,
                scanned,
                errors,
                elapsed,
                cancelled,
            } => {
                if !self.finders_done(id, scanned) {
                    self.on_search_done(id, scanned, errors, elapsed, cancelled);
                }
            }
            AppEvent::GrepPreview { loc, result } => self.on_grep_preview(loc, result),
            AppEvent::TextPreview { loc, result } => self.on_text_preview(loc, result),
        }
    }

    fn on_keymap_loaded(&mut self, result: Result<(Keymap, Vec<String>), String>) {
        match result {
            Ok((map, problems)) => {
                self.keymap = map;
                for p in &problems {
                    tracing::warn!(problem = %p, "keymap.toml");
                }
                match problems.as_slice() {
                    [] => tracing::info!("keymap.toml loaded"),
                    [one] => self.set_status(one.clone(), true),
                    [first, rest @ ..] => {
                        self.set_status(format!("{first} (+{} more in the log)", rest.len()), true)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "keymap.toml not loaded; using defaults");
                self.set_status(format!("{e}; using the default keys"), true);
            }
        }
    }

    fn on_listed(&mut self, id: u64, loc: Loc, result: Result<Arc<Listing>, ListError>) {
        if self.pending.get(&loc).is_none_or(|r| r.id != id) {
            return; // cancelled or superseded
        }
        self.pending.remove(&loc);
        // `:e <file>` or a file as the start path: show it in its directory instead.
        if loc == self.cwd
            && matches!(result, Err(ListError::NotADirectory))
            && let Some(parent) = loc.parent()
        {
            self.dirs.remove(&loc);
            // The file was never a real cwd: `''` should skip it.
            let prev = self.prev_dir.take();
            self.set_cwd(parent, Some(loc.name()));
            self.prev_dir = prev;
            return;
        }
        let state = self.dirs.entry(loc.clone()).or_default();
        state.loading = None;
        let ok = result.is_ok();
        match result {
            Ok(listing) => {
                state.listing = Some(listing);
                state.error = None;
            }
            Err(e) => {
                tracing::info!(%loc, error = %e, "listing failed");
                state.error = Some(e);
            }
        }
        if loc == self.cwd {
            self.rebuild_view();
            // A share that refused the login: offer to connect with other credentials.
            if let Some(err) = self.dirs.get(&loc).and_then(|s| s.error.clone()) {
                self.maybe_ask_credentials(&loc, &err);
            }
        }
        if ok {
            self.remember_share(&loc);
            if loc == Loc::Root && self.at_root() {
                self.probe_root(false);
            }
        }
        self.completion_listed(&loc);
        self.evict_cache();
        self.dirty = true;
    }

    /// Time-driven work: spinner frames, debounced preview, status expiry.
    pub fn tick(&mut self, now: Instant) {
        // The folder order: one save after a burst of `,` keys, in order.
        if self
            .sort_dirty
            .is_some_and(|t| now.duration_since(t) >= Duration::from_millis(500))
        {
            self.sort_dirty = None;
            self.worker.save_ui_state(crate::state::UiState::new(
                self.sort,
                crate::ui::theme::scheme(),
            ));
        }
        // Folder visits: written at most every 5 s, not on every move.
        if self.usage_dirty && now.duration_since(self.usage_saved) >= Duration::from_secs(5) {
            self.usage_dirty = false;
            self.usage_saved = now;
            self.worker.save_usage(self.usage.clone());
        }
        self.finders_tick(now);
        self.apply_pending_pins();
        if self.is_busy() {
            let frame = self.spinner_frame(now);
            if frame != self.last_spin_frame {
                self.last_spin_frame = frame;
                self.dirty = true;
            }
        }
        if self.at_root()
            && self
                .last_probe_round
                .is_none_or(|t| now.duration_since(t) >= REPROBE_EVERY)
        {
            self.probe_root(true);
        }
        self.key_timeout(now);
        self.jobs_tick(now);
        if self.picker.as_mut().is_some_and(|p| p.tick()) {
            self.dirty = true;
        }
        self.image_tick(now);
        self.search_tick(now);
        if self.preview_due.is_some_and(|due| now >= due) {
            self.preview_due = None;
            if let Some(p) = self.preview_loc() {
                self.request(p, false);
            }
            self.request_grep_preview();
            self.request_text_preview();
            if self.mode == Mode::Picker {
                self.probe_picked_machine();
            }
            self.cancel_unneeded();
            self.dirty = true;
        }
        if self
            .status
            .as_ref()
            .is_some_and(|s| now.duration_since(s.at) >= STATUS_TTL)
        {
            self.status = None;
            self.dirty = true;
        }
    }

    /// How long the loop may sleep before something time-driven needs doing.
    fn next_wake(&self, now: Instant) -> Duration {
        let mut wake = IDLE_WAKE;
        if self.is_busy()
            || self.picker.as_ref().is_some_and(|p| p.running)
            || self.jobs_running() > 0
        {
            wake = wake.min(SPINNER_TICK);
        }
        if let Some(due) = self.preview_due {
            wake = wake.min(due.saturating_duration_since(now));
        }
        // Wake when the cursor has been still long enough for a picture.
        if self.image_due > now {
            wake = wake.min(self.image_due - now);
        }
        if let Some(t) = self.keys_since.filter(|_| !self.which_key) {
            wake = wake.min((t + self.keymap.timeout).saturating_duration_since(now));
        }
        wake
    }
}

/// Indices of the entries to show, after the hidden toggle and the `f` filter, which
/// speaks the search's language (`*.pdf`, `ext:pdf`, `size:>10mb`, plain text = a
/// substring of the name; see `search::filter`).
pub fn visible_indices(listing: &Listing, show_hidden: bool, filter: &str) -> Vec<usize> {
    let shown = |e: &&crate::fs::Entry, hidden: bool| hidden || !e.is_hidden();
    let (Some(f), _) = crate::search::filter::DirFilter::new(filter) else {
        return (0..listing.entries.len())
            .filter(|&i| shown(&&listing.entries[i], show_hidden))
            .collect();
    };
    let show_hidden = show_hidden || f.hidden;
    let now = std::time::SystemTime::now();
    let mut scratch = crate::search::name::Scratch::default();
    listing
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| shown(e, show_hidden) && f.keeps(e, now, &mut scratch))
        .map(|(i, _)| i)
        .collect()
}

/// The UI loop: drain events, tick, draw when dirty (capped at ~60 fps), sleep until the
/// next event or the next time-driven change. Returns how the user quit.
pub fn run(
    terminal: &mut Term,
    app: &mut App,
    rx: &EventRx,
    term: &dyn Suspend,
) -> anyhow::Result<Quit> {
    // The frame cap allows two frames per `FRAME`, not one: the answer to a key (a
    // listing, "copied the path") arrives a moment after the key's own frame and is
    // shown at once instead of a frame later. A stream of changes still settles at
    // two draws per 16 ms.
    let mut last_draw = Instant::now() - FRAME;
    let mut draw_before = last_draw;
    // Where the last frame put a picture, and which one (by address).
    let mut last_image: Option<ratatui::layout::Rect> = None;
    let mut last_picture: usize = 0;
    let mut first_frame = true;
    loop {
        let now = Instant::now();
        app.tick(now);
        if !app.dirty {
            // The keys handled changed nothing on screen: no frame to time them by.
            let _ = crate::event::take_input();
        }
        if app.dirty && now.duration_since(draw_before) >= FRAME {
            app.image_drawn = None;
            app.image_place = None;
            let frame = terminal.draw(|f| ui::draw(f, app))?;
            let drawn = app.image_drawn;
            let place = app.image_place.take();
            let picture = place.as_ref().map_or(0, |p| Arc::as_ptr(p) as usize);
            let changed = drawn != last_image || picture != last_picture || app.image_resized;
            // 1. An iTerm2 picture stays until its cells are written again, and ratatui
            //    skips cells it thinks are unchanged. So when the picture moved or went
            //    away, write the old picture's cells again from this frame. On the first
            //    frame and after a resize, write every cell: that also clears pictures
            //    left by whatever ran in this terminal before.
            let old = if first_frame || app.image_resized {
                Some(frame.area)
            } else {
                last_image.filter(|_| changed)
            };
            let repaint: Vec<(u16, u16, ratatui::buffer::Cell)> = match old {
                Some(old) => crate::preview::repaint_cells(old, drawn)
                    .into_iter()
                    .filter_map(|(x, y)| Some((x, y, frame.buffer.cell((x, y))?.clone())))
                    .collect(),
                None => Vec::new(),
            };
            first_frame = false;
            if !repaint.is_empty() {
                use ratatui::backend::Backend;
                let backend = terminal.backend_mut();
                backend.draw(repaint.iter().map(|(x, y, c)| (*x, *y, c)))?;
                backend.flush()?;
            }
            // 2. A new iTerm2 picture is written after the frame, with the cursor shown
            //    so ConPTY really moves it there (crate::preview docs). Only when it
            //    changed: the frames after leave its cells alone.
            if changed
                && let (Some(rect), Some(p)) = (drawn, place.as_ref())
                && let Some(data) = crate::preview::iterm2_data(&p.picture)
            {
                let start = Instant::now();
                crate::preview::place_iterm2(terminal.backend_mut(), rect, data)?;
                tracing::debug!(
                    target: "latency",
                    kb = data.len() / 1024,
                    write_ms = start.elapsed().as_secs_f64() * 1e3,
                    "picture sent"
                );
            }
            last_image = drawn;
            last_picture = picture;
            app.image_resized = false;
            app.dirty = false;
            draw_before = last_draw;
            last_draw = Instant::now();
            // `SENTIER_LOG=latency=debug`: from the key being read to its frame handed
            // to the terminal, and the part of that spent building and writing it.
            if let Some(key_to_frame) = crate::event::take_input() {
                tracing::debug!(
                    target: "latency",
                    key_to_frame_ms = key_to_frame.as_secs_f64() * 1e3,
                    draw_ms = last_draw.duration_since(now).as_secs_f64() * 1e3,
                    "frame"
                );
            }
        }
        if let Some(q) = app.quit {
            return Ok(q);
        }

        let mut wait = app.next_wake(now);
        if app.dirty {
            // Something changed but we're inside the frame cap: draw as soon as allowed.
            wait = wait.min((draw_before + FRAME).saturating_duration_since(now));
        }
        match rx.recv_timeout(wait) {
            Ok(ev) => app.handle(ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(Quit::Plain),
        }
        while let Ok(ev) = rx.try_recv() {
            app.handle(ev);
        }
        // `<C-l>`: whatever else wrote on the screen goes; everything is drawn again.
        if std::mem::take(&mut app.redraw) {
            terminal.clear()?;
            first_frame = true;
            last_image = None;
        }
        // `:rename` (and later inline openers): the program gets the terminal.
        if let Some(req) = app.inline.take() {
            let result = fileops::run_inline(&req, term);
            terminal.clear()?;
            first_frame = true;
            last_image = None;
            app.inline_done(req.then, result);
        }
    }
}

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod job_tests;
#[cfg(test)]
mod machine_tests;
#[cfg(test)]
mod pin_tests;
#[cfg(test)]
mod preview_tests;
#[cfg(test)]
mod tests;
