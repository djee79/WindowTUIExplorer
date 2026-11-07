//! App state and the UI event loop. Key dispatch, actions and prompts live in
//! [`keys`].
//!
//! This module never touches the filesystem. It asks the [`Worker`] for things and reacts
//! to the [`AppEvent`]s that come back.

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
mod rename;
pub use rename::{Field as RenameField, RenamePanel};
mod preview;
mod search;
mod sync;
pub use sync::{PreviewStatus, SyncPreviewState};
mod tabs;
mod tags;

pub use keys::{Prompt, PromptKind};
pub use search::SearchState;

use crate::event::{AppEvent, EventRx};
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::{ListError, Probe, Request, Worker};
use crate::fs::{Entry, EntryKind, Listing};
use crate::keymap::{Action, Key, Keymap};
use crate::platform::DiskSpace;
use crate::ui;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{Event, KeyEventKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant, SystemTime};

/// ~60 fps cap.
const FRAME: Duration = Duration::from_millis(16);
/// Spinner frame rate, only while something is in flight.
pub const SPINNER_TICK: Duration = Duration::from_millis(80);
/// Wait this long after the cursor stops before listing the previewed directory, so
/// holding `j` doesn't fire a request per row.
const PREVIEW_DELAY: Duration = Duration::from_millis(40);
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
    /// Pick mode: a preset folder chosen by going there (`p` takes it).
    pub pick: Option<pick::Pick>,
    /// The open picker, its pending count and keys (normal mode).
    pub picker: Option<crate::picker::Picker>,
    picker_count: Option<usize>,
    picker_pending: Option<picker::PickerPending>,
    /// The pin picker's query when it last closed, for `:view save`.
    last_picker_query: String,
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
    pub image_drawn: Option<ratatui::layout::Rect>,
    /// An iTerm2 picture for the UI loop to write at `image_drawn` after the frame.
    pub image_place: Option<Arc<crate::preview::image::ImagePreview>>,
    /// The window was resized: the old picture must be erased by hand.
    pub image_resized: bool,
    /// `<C-l>`: clear the terminal and draw it all again on the next frame.
    pub redraw: bool,
    /// When the cursor last moved (pictures wait for it to be still).
    moved_at: Instant,
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
            shares: Vec::new(),
            shares_loaded: false,
            pins: crate::pins::PinStore::default(),
            pins_loaded: false,
            pin_draft: None,
            pick: None,
            picker: None,
            picker_count: None,
            picker_pending: None,
            last_picker_query: String::new(),
            confirm: None,
            view_draft: None,
            tag_draft: None,
            graphics: crate::preview::graphics(),
            image: None,
            image_cache: crate::preview::Lru::new(crate::preview::CACHE_ENTRIES),
            image_area: None,
            image_drawn: None,
            image_place: None,
            image_resized: false,
            redraw: false,
            moved_at: Instant::now(),
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
        self.which_key && !self.keys_pending.is_empty()
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
        self.preview_due = Some(now + PREVIEW_DELAY);
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
            self.worker
                .save_ui_state(crate::state::UiState::from_sort(self.sort));
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
        let still = self.moved_at + preview::IMAGE_STILL;
        if still > now {
            wake = wake.min(still - now);
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
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &EventRx,
    term: &dyn Suspend,
) -> anyhow::Result<Quit> {
    let mut last_draw = Instant::now() - FRAME;
    // Where the last frame put a picture, and which one (by address).
    let mut last_image: Option<ratatui::layout::Rect> = None;
    let mut last_picture: usize = 0;
    let mut first_frame = true;
    loop {
        let now = Instant::now();
        app.tick(now);
        if app.dirty && now.duration_since(last_draw) >= FRAME {
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
                crate::preview::place_iterm2(terminal.backend_mut(), rect, data)?;
            }
            last_image = drawn;
            last_picture = picture;
            app.image_resized = false;
            app.dirty = false;
            last_draw = Instant::now();
        }
        if let Some(q) = app.quit {
            return Ok(q);
        }

        let mut wait = app.next_wake(now);
        if app.dirty {
            // Something changed but we're inside the frame cap: draw as soon as allowed.
            wait = wait.min((last_draw + FRAME).saturating_duration_since(now));
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
mod tests {
    use super::*;
    use crate::fs::worker::Timeouts;
    use crate::fs::{EntryKind, sort_entries};
    use crate::platform::Attrs;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    pub(super) fn entry(name: &str, kind: EntryKind, size: u64, hidden: bool) -> Entry {
        Entry {
            name: name.to_string(),
            kind,
            is_link: false,
            size,
            modified: None,
            created: None,
            attrs: Attrs {
                hidden,
                ..Attrs::default()
            },
            detail: String::new(),
        }
    }

    pub(super) fn listing(mut entries: Vec<Entry>) -> Arc<Listing> {
        sort_entries(&mut entries);
        Arc::new(Listing {
            entries,
            loaded_at: Instant::now(),
        })
    }

    pub(super) fn render(app: &mut App, w: u16, h: u16) -> String {
        // TestBackend can't fail: its error type is Infallible.
        let Ok(mut term) = Terminal::new(TestBackend::new(w, h));
        let Ok(_) = term.draw(|f| ui::draw(f, app));
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Builds an app at `C:\work` with canned listings (no real I/O).
    pub(super) fn fixture() -> (App, tokio::runtime::Runtime) {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
        else {
            panic!("runtime")
        };
        let (tx, _rx) = crate::event::channel();
        let worker = Worker::new(rt.handle().clone(), tx, Timeouts::default());
        let Ok(cwd) = Loc::parse(r"C:\work") else {
            panic!()
        };
        let mut app = App::new(worker, cwd.clone());
        app.pending.clear();
        let Some(parent) = cwd.parent() else { panic!() };
        let set = |app: &mut App, loc: &Loc, l: Arc<Listing>| {
            app.dirs.insert(
                loc.clone(),
                DirState {
                    listing: Some(l),
                    ..DirState::default()
                },
            );
        };
        set(
            &mut app,
            &parent,
            listing(vec![
                entry("Users", EntryKind::Dir, 0, false),
                entry("work", EntryKind::Dir, 0, false),
                entry("pagefile.sys", EntryKind::File, 1 << 33, true),
            ]),
        );
        set(
            &mut app,
            &cwd,
            listing(vec![
                entry("HIT", EntryKind::Dir, 0, false),
                entry("notes.txt", EntryKind::File, 1536, false),
                entry("spec2.xlsx", EntryKind::File, 3_800_000, false),
                entry("spec10.xlsx", EntryKind::File, 12, false),
                entry("desktop.ini", EntryKind::File, 282, true),
            ]),
        );
        let Ok(hit) = cwd.join("HIT") else { panic!() };
        set(
            &mut app,
            &hit,
            listing(vec![
                entry("evars.bat", EntryKind::File, 900, false),
                entry("hit000", EntryKind::Dir, 0, false),
            ]),
        );
        app.rebuild_view();
        (app, rt)
    }

    #[test]
    fn renders_three_columns() {
        let (mut app, _rt) = fixture();
        let screen = render(&mut app, 120, 12);
        println!("{screen}");
        assert!(screen.contains(r" sentier   C:\work"), "top bar");
        assert!(
            screen.contains("hit000\\"),
            "preview lists HIT, dirs end in \\"
        );
        assert!(screen.contains("3.6 MB"), "file size column");
        assert!(screen.contains("Name"), "column header");
        assert!(screen.contains("4 items"), "item count in panel title");
        assert!(screen.contains("hidden: off"), "status line");
        assert!(screen.contains("hjkl move"), "key hints");
        assert!(!screen.contains("desktop.ini"), "hidden file hidden");
        assert!(!screen.contains("pagefile.sys"), "hidden in parent too");
        // Natural sort: spec2 before spec10.
        let (a, b) = (screen.find("spec2"), screen.find("spec10"));
        assert!(a.is_some() && a < b, "natural order");
    }

    /// Types keys in vim notation (`gg`, `<Esc>`, `<CR>`, `<C-d>`, `<Space>`).
    pub(super) fn press(app: &mut App, keys: &str) {
        let space = crate::keymap::Key::plain(KeyCode::Char(' '));
        let Ok(keys) = crate::keymap::parse_keys(keys, space) else {
            panic!("keys {keys}")
        };
        for k in keys {
            let mut mods = KeyModifiers::NONE;
            if k.ctrl {
                mods |= KeyModifiers::CONTROL;
            }
            if k.alt {
                mods |= KeyModifiers::ALT;
            }
            if k.shift {
                mods |= KeyModifiers::SHIFT;
            }
            app.handle(AppEvent::Term(Event::Key(KeyEvent::new(k.code, mods))));
        }
    }

    fn sel(app: &App) -> Option<&str> {
        app.selected().map(|e| e.name.as_str())
    }

    /// `<C-l>` and `:redraw`: the next frame clears the terminal first.
    #[test]
    fn ctrl_l_and_redraw_ask_for_a_full_redraw() {
        let (mut app, _rt) = fixture();
        assert!(!app.redraw);
        press(&mut app, "<C-l>");
        assert!(app.redraw, "Ctrl-L");
        app.redraw = false;
        press(&mut app, ":redraw<CR>");
        assert!(app.redraw, ":redraw");
    }

    /// `g?`: generated from the keymap, grouped, chips, commands with their keys; `/`
    /// filters; j/k scroll; any other key closes.
    #[test]
    fn help_overlay_groups_filters_and_closes() {
        let (mut app, _rt) = fixture();
        press(&mut app, "g?");
        assert_eq!(app.mode, Mode::Help);
        let wide = render(&mut app, 220, 60);
        if std::env::var_os("SENTIER_SHOW_HELP").is_some() {
            eprintln!("{wide}");
            eprintln!("{}", render(&mut app, 150, 45));
        }
        for want in [
            "help · ",
            "MOVE",
            "SELECT & EDIT",
            ": GO & CONNECT",
            " ␣ff ",
            ":picker files",
            "type to filter keys and commands",
            "remap in",
        ] {
            assert!(wide.contains(want), "{want}\n{wide}");
        }
        // Small terminals: one column, scrolled, nothing cut mid-row.
        let small = render(&mut app, 80, 24);
        assert!(
            small.contains("MOVE") && small.contains("help · "),
            "{small}"
        );
        if std::env::var_os("SENTIER_SHOW_HELP").is_some() {
            eprintln!("{small}");
        }
        let _ = render(&mut app, 40, 12);
        // Filter: only matching sections stay.
        press(&mut app, "/pdf");
        let f = render(&mut app, 220, 60);
        assert!(f.contains("PREVIEW") && !f.contains("MOVE"), "{f}");
        press(&mut app, "<Esc>");
        assert_eq!(app.mode, Mode::Help, "Esc leaves the filter first");
        press(&mut app, "j");
        assert_eq!(app.mode, Mode::Help, "j scrolls");
        press(&mut app, "x");
        assert_eq!(app.mode, Mode::Normal, "any other key closes");
        assert!(app.help.is_none());
    }

    #[test]
    fn keys_navigate_filter_and_toggle_hidden() {
        let (mut app, _rt) = fixture();
        press(&mut app, "G");
        assert_eq!(sel(&app), Some("spec10.xlsx"));

        press(&mut app, "g.");
        assert_eq!(app.view.len(), 5, "hidden shown");
        assert_eq!(
            sel(&app),
            Some("spec10.xlsx"),
            "selection kept by name across view rebuilds"
        );

        press(&mut app, "fxl");
        assert_eq!(app.view.len(), 2, "filter 'xl' matches the two specs");
        press(&mut app, "<CR>");
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.view.len(), 2, "Enter keeps the filter");
        press(&mut app, "<Esc>");
        assert_eq!(app.view.len(), 5, "Esc in normal mode clears it");

        press(&mut app, "fspec<Esc><Esc>");
        assert_eq!(app.view.len(), 5, "Esc Esc cancels the filter prompt");

        press(&mut app, "gg");
        assert_eq!(sel(&app), Some("HIT"));

        // Enter HIT (cached), then go back up: cursor returns to HIT.
        press(&mut app, "l");
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
        press(&mut app, "-");
        assert_eq!(app.cwd.display(), r"C:\work");
        assert_eq!(sel(&app), Some("HIT"));
        press(&mut app, "o");
        assert_eq!(app.cwd.display(), r"C:\work\HIT", "o on a dir enters it");
        press(&mut app, "h");

        press(&mut app, "ZZ");
        assert_eq!(app.quit, Some(Quit::WithCwd));
    }

    #[test]
    fn counts_and_line_jumps() {
        let (mut app, _rt) = fixture();
        press(&mut app, "2j");
        assert_eq!(app.cursor, 2);
        press(&mut app, "k");
        assert_eq!(app.cursor, 1);
        press(&mut app, "4G");
        assert_eq!(app.cursor, 3, "{{n}}G is 1-based");
        press(&mut app, "1gg");
        assert_eq!(app.cursor, 0);
        press(&mut app, "99j");
        assert_eq!(app.cursor, 3, "clamped to the last row");
        press(&mut app, ":2<CR>");
        assert_eq!(app.cursor, 1, ":{{n}}");
        press(&mut app, "3<Esc>j");
        assert_eq!(app.cursor, 2, "Esc drops a pending count");
        assert_eq!(app.count, None);
    }

    #[test]
    fn search_is_incremental_and_repeats() {
        let (mut app, _rt) = fixture();
        press(&mut app, "/spec");
        assert_eq!(sel(&app), Some("spec2.xlsx"), "jumps while typing");
        press(&mut app, "<CR>");
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.view.len(), 4, "search doesn't filter");
        press(&mut app, "n");
        assert_eq!(sel(&app), Some("spec10.xlsx"));
        press(&mut app, "n");
        assert_eq!(sel(&app), Some("spec2.xlsx"), "wraps around");
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("BOTTOM"))
        );
        press(&mut app, "N");
        assert_eq!(sel(&app), Some("spec10.xlsx"));
        assert!(app.is_search_match("spec2.xlsx"));
        press(&mut app, "<Esc>");
        assert!(
            !app.is_search_match("spec2.xlsx"),
            "Esc clears the highlight"
        );

        // Smartcase: an uppercase letter makes it case-sensitive.
        press(&mut app, "gg/Spec<CR>");
        assert!(app.status.as_ref().is_some_and(|s| s.is_error), "no match");
        // A cancelled search restores the cursor and the previous pattern.
        press(&mut app, "gg/notes<Esc><Esc>");
        assert_eq!(sel(&app), Some("HIT"));
        assert_eq!(app.search.as_ref().map(|s| s.query.as_str()), Some("Spec"));
        press(&mut app, "?xlsx<CR>");
        assert_eq!(sel(&app), Some("spec10.xlsx"), "? searches backward");
    }

    #[test]
    fn history_and_previous_dir() {
        let (mut app, _rt) = fixture();
        press(&mut app, "l");
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
        press(&mut app, "H");
        assert_eq!(app.cwd.display(), r"C:\work");
        press(&mut app, "L");
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
        press(&mut app, "<C-o>");
        assert_eq!(app.cwd.display(), r"C:\work");
        press(&mut app, "''");
        assert_eq!(app.cwd.display(), r"C:\work\HIT", "'' toggles back");
        press(&mut app, "''");
        assert_eq!(app.cwd.display(), r"C:\work");
        press(&mut app, r"g\");
        assert_eq!(app.cwd.display(), r"C:\");
        press(&mut app, "gl");
        assert_eq!(app.cwd, Loc::Root);
    }

    /// Commands don't have to be remembered: `:` lists them as you type, `g?` lists them.
    #[test]
    fn command_line_and_help_list_the_commands() {
        let (mut app, _rt) = fixture();
        press(&mut app, ":");
        let screen = render(&mut app, 200, 30);
        assert!(screen.contains("cd · connect · net · machines"), "{screen}");
        press(&mut app, "pin u");
        let screen = render(&mut app, 200, 30);
        assert!(screen.contains("pin url [URL] — pin a web app"), "{screen}");
        press(&mut app, "<Esc><Esc>g?");
        let screen = render(&mut app, 220, 60);
        assert!(screen.contains(": GO & CONNECT"), "{screen}");
        assert!(screen.contains(":pin url"), "{screen}");
    }

    #[test]
    fn command_line_cd_complete_and_errors() {
        let (mut app, _rt) = fixture();
        press(&mut app, ":cd H<Tab>");
        let text = app.prompt.as_ref().map(|p| p.editor.text());
        assert_eq!(
            text.as_deref(),
            Some(r"cd HIT\"),
            "completed from the cache"
        );
        press(&mut app, "<CR>");
        assert_eq!(app.cwd.display(), r"C:\work\HIT");

        press(&mut app, ":cd ..<CR>");
        assert_eq!(app.cwd.display(), r"C:\work");

        // `g<Space>`: the same prompt, with `cd ` already typed.
        press(&mut app, "g<Space>");
        let text = app.prompt.as_ref().map(|p| p.editor.text());
        assert_eq!(text.as_deref(), Some("cd "));
        press(&mut app, "HIT<CR>");
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
        press(&mut app, ":cd ..<CR>");

        press(&mut app, ":frobnicate<CR>");
        assert!(app.status.as_ref().is_some_and(|s| s.is_error));
        press(&mut app, ":picker nope<CR>");
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("picker files | dirs"))
        );
        press(&mut app, ":set nornu<CR>");
        assert!(!app.relative_number && app.number);
        press(&mut app, ":q<CR>");
        assert_eq!(app.quit, Some(Quit::Plain));
    }

    #[test]
    fn which_key_waits_then_shows() {
        let (mut app, _rt) = fixture();
        press(&mut app, "g");
        assert!(!app.which_key_visible(), "not before the timeout");
        app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
        assert!(app.which_key_visible());
        let screen = render(&mut app, 120, 20);
        assert!(
            screen.contains("+") || screen.contains("Locations"),
            "{screen}"
        );
        assert!(screen.contains("l → Locations"), "{screen}");
        press(&mut app, "<Esc>");
        assert!(!app.which_key_visible());
        assert!(app.keys_pending.is_empty());

        press(&mut app, "<Space>");
        app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
        let screen = render(&mut app, 120, 20);
        assert!(screen.contains("<Space>"), "leader title: {screen}");
        press(&mut app, "l");
        assert_eq!(app.cwd, Loc::Root, "<leader>l");

        press(&mut app, "gq");
        assert!(app.keys_pending.is_empty(), "unknown sequence clears");
    }

    fn hit(rel: &str, score: u32, size: u64) -> crate::search::walk::Hit {
        crate::search::walk::Hit {
            rel: rel.to_string(),
            is_dir: false,
            is_link: false,
            size,
            modified: None,
            created: None,
            attrs: Attrs::default(),
            score,
        }
    }

    /// `s` from C:\work, with hits fed in as the walker would send them.
    fn results_fixture() -> (App, tokio::runtime::Runtime, u64) {
        let (mut app, rt) = fixture();
        press(&mut app, "sb1001<CR>");
        let Loc::Results { id, .. } = app.cwd else {
            panic!("not in results: {}", app.cwd)
        };
        app.handle(AppEvent::SearchProgress {
            id,
            hits: vec![
                hit(r"HIT\iso\B-1001_rev2.pdf", 80, 3),
                hit(r"HIT\iso\B-1001_rev3.pdf", 98, 1),
            ],
            scanned: 1200,
            current: r"HIT\iso".to_string(),
        });
        // A duplicate from another walker thread is ignored.
        app.handle(AppEvent::SearchProgress {
            id,
            hits: vec![
                hit(r"hit\ISO\b-1001_rev3.pdf", 98, 1),
                hit(r"HIT\B1001.txt", 60, 2),
            ],
            scanned: 2400,
            current: String::new(),
        });
        app.tick(Instant::now() + Duration::from_millis(60));
        (app, rt, id)
    }

    #[test]
    fn search_results_are_a_folder() {
        let (mut app, _rt, id) = results_fixture();
        assert_eq!(app.view.len(), 3, "deduplicated");
        assert_eq!(
            sel(&app),
            Some(r"HIT\iso\B-1001_rev3.pdf"),
            "best score first"
        );
        assert_eq!(
            app.cwd.parent(),
            Some(loc(r"C:\work")),
            "parent is the origin"
        );

        let screen = render(&mut app, 140, 14);
        println!("{screen}");
        assert!(screen.contains("search \"b1001\"  in  C:\\work"), "header");
        assert!(screen.contains("2 400 scanned · 3 found"), "progress");
        assert!(screen.contains("search: b1001"), "panel title");
        assert!(
            screen.contains("Path") && screen.contains("Score"),
            "columns"
        );
        assert!(screen.contains("walking HIT\\iso"), "current dir");

        assert!(
            screen.contains("Found in  C:\\work\\HIT\\iso\\"),
            "real folder"
        );

        // `,s` cycles the sort (the cursor stays on its item); size puts 3 B first.
        press(&mut app, ",s,s");
        assert_eq!(sel(&app), Some(r"HIT\iso\B-1001_rev3.pdf"), "cursor kept");
        press(&mut app, "gg");
        assert_eq!(sel(&app), Some(r"HIT\iso\B-1001_rev2.pdf"), "sort: size");

        // Esc while running cancels the walk but keeps the results.
        press(&mut app, "<Esc>");
        assert!(matches!(app.cwd, Loc::Results { .. }));
        app.handle(AppEvent::SearchDone {
            id,
            scanned: 2400,
            errors: 0,
            elapsed: Duration::from_millis(2100),
            cancelled: true,
        });
        assert_eq!(app.view.len(), 3);
        // Esc again goes back to where the search started; L returns to the results.
        press(&mut app, "<Esc>");
        assert_eq!(app.cwd, loc(r"C:\work"));
        press(&mut app, "L");
        assert!(matches!(app.cwd, Loc::Results { .. }));
        assert_eq!(app.view.len(), 3, "results kept in history");

        // gr: the real folder of the result, with it selected.
        press(&mut app, "gg");
        let name = sel(&app).map(str::to_string);
        press(&mut app, "gr");
        assert_eq!(app.cwd, loc(r"C:\work\HIT\iso"), "from {name:?}");
        assert_eq!(app.remembered(&app.cwd), Some("B-1001_rev2.pdf"));
    }

    #[test]
    fn search_refines_and_reports_bad_queries() {
        let (mut app, _rt, first) = results_fixture();
        // `s` inside results is prefilled with the query; Enter starts a fresh search.
        press(&mut app, "s");
        assert_eq!(
            app.prompt.as_ref().map(|p| p.editor.text()).as_deref(),
            Some("b1001")
        );
        press(&mut app, " rev3<CR>");
        let Loc::Results { id, ref origin } = app.cwd else {
            panic!()
        };
        assert_ne!(id, first, "a new search");
        assert_eq!(**origin, loc(r"C:\work"), "same origin, not nested");
        assert_eq!(
            app.current_search_query(false).as_deref(),
            Some("b1001 rev3")
        );

        press(&mut app, "s<C-u>size:huge<CR>");
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.is_error && s.text.contains("use < or >"))
        );
        press(&mut app, "gl");
        press(&mut app, "sx<CR>");
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("inside a folder")),
            "no search from Locations"
        );
    }

    #[test]
    fn hybrid_line_numbers() {
        let (mut app, _rt) = fixture();
        press(&mut app, "2j");
        let screen = render(&mut app, 120, 12);
        println!("{screen}");
        // Cursor row: absolute, left-aligned. Others: distance, right-aligned.
        assert!(screen.contains("   2 HIT\\"), "{screen}");
        assert!(screen.contains("│ 3   spec2.xlsx"), "{screen}");
        assert!(screen.contains("   1 spec10.xlsx"), "{screen}");
        press(&mut app, ":set nonu<CR>:set nornu<CR>");
        let screen = render(&mut app, 120, 12);
        assert!(screen.contains("│ spec2.xlsx"), "gutter off: {screen}");
    }

    fn loc(s: &str) -> Loc {
        let Ok(l) = Loc::parse(s) else { panic!("{s}") };
        l
    }

    /// A share that won't open: if its server answers on port 445, the login screen opens
    /// by itself (once per server); if not, it says the server is down. No `:connect`.
    #[test]
    fn unreachable_share_offers_the_login_when_the_server_answers() {
        let at = |app: &App, name: &str| {
            let Some(l) = &app.view_listing else { panic!() };
            app.view
                .iter()
                .position(|&i| l.entries[i].name == name)
                .unwrap_or_else(|| panic!("{name}"))
        };
        let nas = r"\\nas02\archive";
        let text = |app: &App| {
            app.status
                .as_ref()
                .map(|s| s.text.clone())
                .unwrap_or_default()
        };

        // Down: a plain message, no login screen.
        let (mut app, _rt) = root_fixture();
        app.set_cursor(at(&app, nas));
        press(&mut app, "<CR>");
        assert!(
            text(&app).contains("checking whether nas02 answers"),
            "{}",
            text(&app)
        );
        app.handle(AppEvent::SmbAnswer {
            open: nas.into(),
            up: false,
        });
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            text(&app).contains("doesn't answer on SMB"),
            "{}",
            text(&app)
        );

        // Up: the login screen, and the share opens once connected.
        press(&mut app, "<CR>");
        app.handle(AppEvent::SmbAnswer {
            open: nas.into(),
            up: true,
        });
        assert_eq!(app.mode, Mode::Credentials);
        assert!(
            app.cred
                .as_ref()
                .is_some_and(|c| c.reason.contains("answers but didn't let you in"))
        );
        // Esc: not asked again for that server; the message says how instead.
        press(&mut app, "<Esc>");
        press(&mut app, "<CR>");
        assert!(text(&app).contains(":connect logs in"), "{}", text(&app));
        assert!(app.smb_check.is_none());

        // An answer that comes after the user moved on is dropped.
        let (mut app, _rt) = root_fixture();
        app.set_cursor(at(&app, nas));
        press(&mut app, "<CR>");
        press(&mut app, "gd");
        app.handle(AppEvent::SmbAnswer {
            open: nas.into(),
            up: true,
        });
        assert_ne!(app.mode, Mode::Credentials);

        // Inside a share whose listing timed out: same check, prompt with no `open`.
        let (mut app, _rt) = fixture();
        let target = loc(r"\\nas03\photos");
        app.navigate(target.clone(), None);
        let Some(id) = app.pending.get(&target).map(|r| r.id) else {
            panic!()
        };
        app.handle(AppEvent::Listed {
            id,
            loc: target,
            result: Err(ListError::TimedOut),
        });
        assert!(app.smb_check.is_some());
        app.handle(AppEvent::SmbAnswer {
            open: r"\\nas03\photos".into(),
            up: true,
        });
        assert_eq!(
            app.cred.as_ref().map(|c| c.target.as_str()),
            Some(r"\\nas03\photos")
        );
    }

    /// `:connect` in Locations logs in to the share (or mapped drive) under the cursor,
    /// then opens it; `:connect <path>` works from anywhere.
    #[test]
    fn connect_from_locations_logs_in_then_opens() {
        let (mut app, _rt) = root_fixture();
        let at = |app: &App, name: &str| {
            let Some(l) = &app.view_listing else { panic!() };
            app.view
                .iter()
                .position(|&i| l.entries[i].name == name)
                .unwrap_or_else(|| panic!("{name}"))
        };
        let i = at(&app, r"\\nas02\archive");
        app.set_cursor(i);
        press(&mut app, ":connect<CR>");
        assert_eq!(app.mode, Mode::Credentials);
        let target = app
            .cred
            .as_ref()
            .map(|c| (c.target.clone(), c.reason.clone()));
        assert_eq!(
            target,
            Some((
                r"\\nas02\archive".to_string(),
                r"log in to open \\nas02\archive".to_string()
            ))
        );
        press(&mut app, "me<Tab>pw<CR>");
        assert!(app.cred.as_ref().is_some_and(|c| c.busy));
        app.handle(AppEvent::Connected {
            requested: r"\\nas02\archive".into(),
            target: r"\\nas02\archive".into(),
            user: "me".into(),
            result: Ok(()),
            remembered: None,
            dropped: Vec::new(),
        });
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.cwd.display(),
            r"\\nas02\archive",
            "opened once logged in"
        );

        // A mapped drive: its UNC target. A path: that share, then that folder.
        press(&mut app, "gl");
        let i = at(&app, "P:");
        app.set_cursor(i);
        press(&mut app, ":connect<CR>");
        assert_eq!(
            app.cred.as_ref().map(|c| c.target.as_str()),
            Some(r"\\srv01\project")
        );
        press(&mut app, "<Esc>");
        press(&mut app, r":connect \\nas02\archive\old<CR>");
        assert_eq!(
            app.cred.as_ref().map(|c| c.target.as_str()),
            Some(r"\\nas02\archive")
        );
        press(&mut app, "<Esc>");
        // C: isn't a share: it says what :connect wants.
        let i = at(&app, "C:");
        app.set_cursor(i);
        press(&mut app, ":connect<CR>");
        assert_ne!(app.mode, Mode::Credentials);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains(r"\\server\share"))
        );
    }

    /// The app at the Locations root with two drives and two saved shares, one of which
    /// is known to be unreachable.
    fn root_fixture() -> (App, tokio::runtime::Runtime) {
        use crate::platform::DriveKind;
        let (mut app, rt) = fixture();
        app.on_shares_loaded(Ok(locations_shares(&[
            r"\\srv01\project",
            r"\\nas02\archive",
        ])));
        let root = listing_unsorted(vec![
            Entry::virtual_entry(
                "C:".into(),
                EntryKind::Drive(DriveKind::Fixed),
                "Windows".into(),
            ),
            Entry::virtual_entry(
                "P:".into(),
                EntryKind::Drive(DriveKind::Network),
                r"\\srv01\project".into(),
            ),
            Entry::virtual_entry(r"\\nas02\archive".into(), EntryKind::Share, String::new()),
            Entry::virtual_entry(r"\\srv01\project".into(), EntryKind::Share, String::new()),
        ]);
        app.dirs.insert(
            Loc::Root,
            DirState {
                listing: Some(root),
                ..DirState::default()
            },
        );
        app.set_cwd(Loc::Root, None);
        app.pending.clear();
        app.on_probed(
            loc(r"C:\"),
            Ok(Probe {
                rtt: Duration::from_millis(1),
                free: Some(412 << 30),
            }),
        );
        app.on_probed(loc(r"\\nas02\archive"), Err(ListError::TimedOut));
        (app, rt)
    }

    fn locations_shares(v: &[&str]) -> Vec<FsPath> {
        v.iter().filter_map(|s| loc(s).as_fs().cloned()).collect()
    }

    pub(super) fn listing_unsorted(entries: Vec<Entry>) -> Arc<Listing> {
        Arc::new(Listing {
            entries,
            loaded_at: Instant::now(),
        })
    }

    #[test]
    fn locations_screen_renders_sections_and_reachability() {
        let (mut app, _rt) = root_fixture();
        let screen = render(&mut app, 160, 24);
        println!("{screen}");
        assert!(
            screen.contains("DRIVES") && screen.contains("SHARES"),
            "sections"
        );
        assert!(screen.contains("● C:  Windows"), "label + reachable dot");
        assert!(
            screen.contains("412.0 GB free"),
            "free space from the probe"
        );
        assert!(
            screen.contains(r"→                  \\srv01\project"),
            "mapped target"
        );
        assert!(screen.contains("mapped"), "mapped drive tag");
        assert!(
            screen.contains(r"× ") && screen.contains("timed out"),
            "dead share"
        );
        assert!(screen.contains("1 of 2 reachable"), "top bar summary");
        assert!(
            screen.contains("4 locations  ·  0 pinned folders  ·  2 drives  ·  2 shares"),
            "status line"
        );
        assert!(screen.contains("re-probed every 60 s"), "legend");
    }

    #[test]
    fn unreachable_location_stays_put() {
        let (mut app, _rt) = root_fixture();
        let key = |c: KeyCode| AppEvent::Term(Event::Key(KeyEvent::from(c)));
        app.handle(key(KeyCode::Char('j')));
        app.handle(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected().map(|e| e.name.as_str()),
            Some(r"\\nas02\archive")
        );
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.cwd, Loc::Root, "did not navigate");
        assert!(app.status.as_ref().is_some_and(|s| s.is_error), "said why");
        assert!(
            app.reach_of(&loc(r"\\nas02\archive"))
                .is_some_and(|r| r.probing),
            "re-probing"
        );
        // A reachable one opens.
        app.handle(key(KeyCode::Char('g')));
        app.handle(key(KeyCode::Char('g')));
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.cwd, loc(r"C:\"));
    }

    #[test]
    fn dd_forgets_a_saved_share_only() {
        let (mut app, _rt) = root_fixture();
        let key = |c: KeyCode| AppEvent::Term(Event::Key(KeyEvent::from(c)));
        // On a drive: refused.
        app.handle(key(KeyCode::Char('d')));
        app.handle(key(KeyCode::Char('d')));
        assert_eq!(app.shares.len(), 2);
        // On the dead share: removed.
        app.handle(key(KeyCode::Char('G')));
        app.handle(key(KeyCode::Char('k')));
        app.handle(key(KeyCode::Char('d')));
        app.handle(key(KeyCode::Char('d')));
        assert_eq!(
            app.shares.iter().map(FsPath::display).collect::<Vec<_>>(),
            [r"\\srv01\project"]
        );
        assert!(app.reach_of(&loc(r"\\nas02\archive")).is_none());
    }

    #[test]
    fn browsing_a_share_saves_it_once() {
        let (mut app, _rt) = fixture();
        app.on_shares_loaded(Ok(Vec::new()));
        for path in [r"\\srv01\project\HIT", r"\\SRV01\Project", r"C:\local"] {
            let l = loc(path);
            let Some(req) = Some(app.worker.list(l.clone())) else {
                panic!()
            };
            let id = req.id;
            app.pending.insert(l.clone(), req);
            app.handle(AppEvent::Listed {
                id,
                loc: l,
                result: Ok(listing(Vec::new())),
            });
        }
        assert_eq!(
            app.shares.iter().map(FsPath::display).collect::<Vec<_>>(),
            [r"\\srv01\project"]
        );
    }

    #[test]
    fn shares_browsed_before_the_store_loads_are_merged() {
        let (mut app, _rt) = fixture();
        app.remember_share(&loc(r"\\new\x\deep"));
        app.on_shares_loaded(Ok(locations_shares(&[r"\\old\y"])));
        assert_eq!(
            app.shares.iter().map(FsPath::display).collect::<Vec<_>>(),
            [r"\\old\y", r"\\new\x"]
        );
    }

    #[test]
    fn stale_results_are_ignored() {
        let (mut app, _rt) = fixture();
        let before = app.view.len();
        app.handle(AppEvent::Listed {
            id: 999_999,
            loc: app.cwd.clone(),
            result: Err(ListError::TimedOut),
        });
        assert_eq!(app.view.len(), before);
        assert!(app.dir(&app.cwd).is_some_and(|s| s.error.is_none()));
    }
}

#[cfg(test)]
mod pin_tests {
    //! The test runtime is never driven, so the worker's pins.toml load and save tasks
    //! never run: these tests can't touch the real %APPDATA%.
    use super::tests::{fixture, listing_unsorted, press, render};
    use super::*;
    use crate::pins::{PinKind, PinStore};

    fn status(app: &App) -> String {
        app.status
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default()
    }

    fn prompt_text(app: &App) -> Option<String> {
        app.prompt.as_ref().map(|p| p.editor.text())
    }

    fn loaded() -> (App, tokio::runtime::Runtime) {
        let (mut app, rt) = fixture();
        app.on_pins_loaded(Ok((PinStore::default(), Vec::new())));
        (app, rt)
    }

    #[test]
    fn mark_names_tags_then_jumps_back() {
        let (mut app, _rt) = loaded();
        press(&mut app, "ma");
        assert_eq!(prompt_text(&app).as_deref(), Some("work"), "name prefilled");
        press(&mut app, "<C-u>Work dir<CR>");
        assert_eq!(prompt_text(&app).as_deref(), Some(""), "tags step");
        press(&mut app, "aveva #ACME<CR>");
        assert!(app.prompt.is_none(), "m{{key}} has its key: no key step");
        let pin = &app.pins.pins[0];
        assert_eq!(
            (pin.name.as_str(), pin.kind, pin.key_char()),
            ("Work dir", PinKind::Folder, Some('a'))
        );
        assert_eq!(pin.tags, ["aveva", "acme"]);
        assert_eq!(pin.path.as_deref(), Some(r"C:\work"));
        assert_eq!(status(&app), "'a → Work dir");

        press(&mut app, ":cd HIT<CR>");
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
        press(&mut app, "'a");
        assert_eq!(app.cwd.display(), r"C:\work", "jumped");
        press(&mut app, "''");
        assert_eq!(app.cwd.display(), r"C:\work\HIT", "'' still goes back");

        // The same folder again: only the key moves, no prompts.
        press(&mut app, "'amb");
        assert!(app.prompt.is_none());
        assert_eq!(app.pins.pins.len(), 1);
        assert_eq!(app.pins.pins[0].key_char(), Some('b'));
        press(&mut app, "'z");
        assert!(status(&app).contains("no pin on 'z"));
    }

    #[test]
    fn pin_item_asks_for_a_key_and_validates_it() {
        let (mut app, _rt) = loaded();
        press(&mut app, "j"); // notes.txt
        press(&mut app, "M");
        assert_eq!(prompt_text(&app).as_deref(), Some("notes.txt"));
        press(&mut app, "<CR><CR>");
        assert_eq!(prompt_text(&app).as_deref(), Some(""), "key step");
        press(&mut app, "x!<CR>");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("letter or digit")),
            "stays open with a note"
        );
        press(&mut app, "<C-u>n<CR>");
        assert!(app.prompt.is_none());
        let pin = &app.pins.pins[0];
        assert_eq!((pin.kind, pin.key_char()), (PinKind::File, Some('n')));
        assert_eq!(pin.path.as_deref(), Some(r"C:\work\notes.txt"));

        press(&mut app, "M");
        assert!(status(&app).contains("already pinned"), "{}", status(&app));
    }

    #[test]
    fn esc_pins_nothing_and_nothing_changes_before_load() {
        let (mut app, _rt) = loaded();
        press(&mut app, "mc<Esc><Esc>");
        assert!(app.prompt.is_none());
        assert!(app.pins.pins.is_empty());
        assert_eq!(status(&app), "not pinned");

        let (mut app, _rt) = fixture();
        press(&mut app, "ma");
        assert!(app.prompt.is_none(), "store not read yet: no prompt");
        assert!(app.status.as_ref().is_some_and(|s| s.is_error));
        press(&mut app, "m!");
        assert!(status(&app).contains("letters or digits"));
    }

    #[test]
    fn tag_completion_most_used_first() {
        let (mut app, _rt) = loaded();
        press(&mut app, "ma<CR>aveva servers<CR>");
        press(&mut app, ":cd HIT<CR>mh<CR>");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("#aveva 1")),
            "candidates shown when the tags step opens"
        );
        press(&mut app, "se<Tab>");
        assert_eq!(prompt_text(&app).as_deref(), Some("servers "));
        press(&mut app, "<CR>");
        assert_eq!(app.pins.pins[1].tags, ["servers"]);
    }

    #[test]
    fn locations_lists_pins_and_dd_asks_first() {
        let (mut app, _rt) = loaded();
        press(&mut app, "ma<CR>aveva<CR>");
        let entries = app.pin_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            (entries[0].name.as_str(), entries[0].detail.as_str()),
            (r"C:\work", "work")
        );
        app.dirs.insert(
            Loc::Root,
            DirState {
                listing: Some(listing_unsorted(entries)),
                ..DirState::default()
            },
        );
        app.set_cwd(Loc::Root, None);
        app.pending.clear();
        let screen = render(&mut app, 160, 24);
        assert!(screen.contains("PINNED FOLDERS"), "{screen}");
        assert!(
            screen.contains("'a") && screen.contains(r"C:\work"),
            "{screen}"
        );
        assert!(screen.contains("#aveva"), "detail panel tags: {screen}");

        press(&mut app, "ddn");
        assert_eq!(app.pins.pins.len(), 1, "anything but y keeps it");
        assert!(status(&app).starts_with("kept"));
        press(&mut app, "T");
        assert_eq!(prompt_text(&app).as_deref(), Some("aveva"));
        press(&mut app, "<C-u>acme<CR>");
        assert_eq!(app.pins.pins[0].tags, ["acme"]);
        press(&mut app, "r<C-u>Work<CR>");
        assert_eq!(app.pins.pins[0].name, "Work");
        press(&mut app, "ddy");
        assert!(app.pins.pins.is_empty());
        assert!(status(&app).starts_with("unpinned"));
    }

    #[test]
    fn which_key_after_quote_lists_pins() {
        let (mut app, _rt) = loaded();
        press(&mut app, "ma<C-u>Projects<CR><CR>");
        press(&mut app, "'");
        app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
        let screen = render(&mut app, 120, 20);
        assert!(screen.contains("Projects"), "{screen}");
        press(&mut app, "<Esc>");
        assert!(!app.which_key_visible());
    }

    /// Lets the picker's matcher threads finish and takes their results.
    fn settle(app: &mut App) {
        let end = Instant::now() + Duration::from_secs(5);
        for _ in 0..3 {
            app.tick(Instant::now());
        }
        while app.picker.as_ref().is_some_and(|p| p.running) && Instant::now() < end {
            app.tick(Instant::now());
        }
    }

    fn picker_names(app: &App) -> Vec<String> {
        app.picker
            .as_ref()
            .map(|p| p.view.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Two pins: `'a` C:\work (aveva), `'h` C:\work\HIT (aveva, servers); cwd C:\work.
    fn two_pins() -> (App, tokio::runtime::Runtime) {
        let (mut app, rt) = loaded();
        press(&mut app, "ma<C-u>Work dir<CR>aveva<CR>");
        press(&mut app, ":cd HIT<CR>mh<CR>aveva servers<CR>");
        press(&mut app, "'a");
        (app, rt)
    }

    #[test]
    fn picker_filters_ranks_and_opens() {
        let (mut app, _rt) = two_pins();
        press(&mut app, "<Space>p");
        assert_eq!(app.mode, Mode::Picker);
        settle(&mut app);
        assert_eq!(picker_names(&app).len(), 2);

        press(&mut app, "hi");
        settle(&mut app);
        assert_eq!(picker_names(&app)[0], "HIT", "name prefix first");

        press(&mut app, "<C-u>#se<Tab>");
        assert_eq!(
            app.picker.as_ref().map(|p| p.editor.text()).as_deref(),
            Some("#servers ")
        );
        settle(&mut app);
        assert_eq!(picker_names(&app), ["HIT"]);

        let screen = render(&mut app, 160, 30);
        assert!(
            screen.contains("[folder]") && screen.contains("#servers"),
            "{screen}"
        );
        assert!(screen.contains("1 / 2"), "counter: {screen}");

        press(&mut app, "<CR>");
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
    }

    #[test]
    fn picker_normal_mode_moves_and_closes() {
        let (mut app, _rt) = two_pins();
        press(&mut app, "<Space>p");
        settle(&mut app);
        press(&mut app, "<Esc>j");
        assert_eq!(app.picker.as_ref().map(|p| p.cursor), Some(1));
        press(&mut app, "gg");
        assert_eq!(app.picker.as_ref().map(|p| p.cursor), Some(0));
        press(&mut app, "]t");
        settle(&mut app);
        assert_eq!(
            app.picker
                .as_ref()
                .and_then(|p| p.query.single_tag().map(str::to_string)),
            Some("aveva".to_string()),
            "most used tag first"
        );
        press(&mut app, "q");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.picker.is_none());
    }

    #[test]
    fn picker_edits_come_back_to_the_picker() {
        let (mut app, _rt) = two_pins();
        press(&mut app, "<Space>phit");
        settle(&mut app);
        press(&mut app, "<C-t>");
        assert_eq!(app.mode, Mode::Prompt);
        press(&mut app, "<C-u>e3d<CR>");
        assert_eq!(app.pins.pins[1].tags, ["e3d"]);
        assert_eq!(app.mode, Mode::Picker, "back in the picker");
        assert_eq!(
            app.picker.as_ref().map(|p| p.editor.text()).as_deref(),
            Some("hit"),
            "same query"
        );

        // ^e: name, tags, then the key (prefilled with the current one).
        settle(&mut app);
        press(&mut app, "<C-e><C-u>HIT server<CR><CR>");
        assert_eq!(
            app.prompt.as_ref().map(|p| p.editor.text()).as_deref(),
            Some("h")
        );
        press(&mut app, "<C-u>x<CR>");
        assert_eq!(app.pins.pins[1].name, "HIT server");
        assert_eq!(app.pins.pins[1].key_char(), Some('x'));

        settle(&mut app);
        press(&mut app, "<C-x>n");
        assert_eq!(app.pins.pins.len(), 2, "n keeps it");
        assert_eq!(app.mode, Mode::Picker);
        settle(&mut app);
        press(&mut app, "<C-x>y");
        assert_eq!(app.pins.pins.len(), 1);
        assert_eq!(app.mode, Mode::Picker);
    }

    fn prompt_note(app: &App) -> String {
        app.prompt
            .as_ref()
            .map(|p| p.note.clone())
            .unwrap_or_default()
    }

    #[test]
    fn views_save_jump_and_delete() {
        let (mut app, _rt) = two_pins();
        press(&mut app, ":view save nothing<CR>");
        assert!(
            status(&app).contains("no query to save"),
            "{}",
            status(&app)
        );

        press(&mut app, "<Space>p#servers<Esc>q");
        assert_eq!(app.last_picker_query, "#servers");
        press(&mut app, ":view save main server<CR>");
        assert_eq!(
            app.prompt.as_ref().map(|p| p.kind),
            Some(PromptKind::ViewKey)
        );
        press(&mut app, "v<CR>");
        let v = &app.pins.views[0];
        assert_eq!(
            (v.name.as_str(), v.query.as_str(), v.key_char()),
            ("main server", "#servers", Some('v'))
        );

        // 'v opens the picker with the view's query.
        press(&mut app, "'v");
        assert_eq!(app.mode, Mode::Picker);
        settle(&mut app);
        assert_eq!(picker_names(&app), ["HIT"]);
        press(&mut app, "<Esc>q");

        // The view is a row in the pin picker; ^x deletes it after a y.
        press(&mut app, "<Space>pis:view");
        settle(&mut app);
        assert_eq!(picker_names(&app), ["main server"]);
        press(&mut app, "<C-x>");
        assert!(prompt_note(&app).contains("delete view main server"));
        press(&mut app, "y");
        assert!(app.pins.views.is_empty());
        assert_eq!(app.mode, Mode::Picker, "back in the picker");
    }

    #[test]
    fn tag_rename_and_delete_ask_first() {
        let (mut app, _rt) = two_pins();
        press(&mut app, ":tag rename aveva e3d<CR>");
        assert!(
            prompt_note(&app).contains("#aveva → #e3d on 2 pins"),
            "{}",
            prompt_note(&app)
        );
        press(&mut app, "y");
        assert_eq!(app.pins.tag_count("e3d"), 2);
        assert_eq!(app.pins.tag_count("aveva"), 0);

        press(&mut app, ":tag delete servers<CR>");
        assert!(prompt_note(&app).contains("from 1 pin"));
        press(&mut app, "n");
        assert_eq!(app.pins.tag_count("servers"), 1, "n keeps it");
        press(&mut app, ":tag delete nope<CR>");
        assert!(status(&app).contains("no pin is tagged #nope"));
    }

    #[test]
    fn tag_picker_chooses_and_renames() {
        let (mut app, _rt) = two_pins();
        press(&mut app, "<Space>t");
        settle(&mut app);
        assert_eq!(picker_names(&app), ["aveva", "servers"], "most used first");
        press(&mut app, "<Tab><Tab><CR>");
        assert_eq!(
            app.picker.as_ref().map(|p| p.editor.text()).as_deref(),
            Some("#aveva #servers "),
            "both chosen tags, all required"
        );
        settle(&mut app);
        assert_eq!(picker_names(&app), ["HIT"]);
        press(&mut app, "<Esc>q");

        press(&mut app, ":tags<CR>");
        settle(&mut app);
        press(&mut app, "<C-e>");
        assert_eq!(prompt_text(&app).as_deref(), Some("aveva"));
        press(&mut app, "<C-u>e3d<CR>y");
        assert_eq!(app.pins.tag_count("e3d"), 2);
        assert_eq!(app.mode, Mode::Picker, "back in the tag picker");
        settle(&mut app);
        assert_eq!(picker_names(&app)[0], "e3d");
    }

    #[test]
    fn locations_filter_takes_the_pin_query() {
        use crate::platform::DriveKind;
        let (mut app, _rt) = two_pins();
        let mut entries = app.pin_entries();
        entries.push(Entry::virtual_entry(
            "C:".into(),
            EntryKind::Drive(DriveKind::Fixed),
            "Windows".into(),
        ));
        entries.push(Entry::virtual_entry(
            r"\\srv01\project".into(),
            EntryKind::Share,
            String::new(),
        ));
        app.dirs.insert(
            Loc::Root,
            DirState {
                listing: Some(listing_unsorted(entries)),
                ..DirState::default()
            },
        );
        app.set_cwd(Loc::Root, None);
        app.pending.clear();
        assert_eq!(app.view.len(), 4);
        let shown = |app: &App| -> Vec<String> {
            app.view_listing
                .as_ref()
                .map(|l| {
                    app.view
                        .iter()
                        .map(|&i| l.entries[i].name.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        press(&mut app, "f#servers<CR>");
        assert_eq!(shown(&app), [r"C:\work\HIT"]);
        press(&mut app, "f<C-u>is:unc<CR>");
        assert_eq!(shown(&app), [r"\\srv01\project"]);
        press(&mut app, "f<C-u>is:local windows<CR>");
        assert_eq!(shown(&app), ["C:"]);
        press(&mut app, "f<C-u>on:srv01<CR>");
        assert_eq!(shown(&app), [r"\\srv01\project"]);
    }

    #[test]
    fn pin_command_pins_or_edits_the_cwd() {
        let (mut app, _rt) = loaded();
        press(&mut app, ":pin<CR>");
        assert_eq!(prompt_text(&app).as_deref(), Some("work"));
        press(&mut app, "<CR><CR>w<CR>");
        assert_eq!(app.pins.pins[0].key_char(), Some('w'));
        press(&mut app, ":pin<CR>");
        assert_eq!(
            prompt_text(&app).as_deref(),
            Some("work"),
            "edits the existing pin"
        );
        press(&mut app, "<Esc><Esc>");
        assert_eq!(app.pins.pins.len(), 1);
    }

    /// `<leader><leader>`: every pin, the most used first with no query (under
    /// "frequent", then groups by first tag); opening counts a use; with no match, Enter
    /// runs the typed text.
    #[test]
    fn launcher_browses_ranks_and_counts_uses() {
        let (mut app, _rt) = loaded();
        let text = r#"
            [[pin]]
            kind = "app"
            name = "E3D HIT"
            command = 'C:\nope\evars.bat'
            tags = ["aveva"]
            [[pin]]
            kind = "web"
            name = "Grafana"
            url = "https://grafana.example"
            tags = ["monitoring"]
            [[pin]]
            kind = "folder"
            name = "Work"
            path = 'C:\work'
        "#;
        let Ok((store, notes)) = crate::pins::parse(text) else {
            panic!()
        };
        assert!(notes.is_empty(), "{notes:?}");
        app.pins = store;
        let now = crate::frecency::now_secs();
        for key in [
            r"c:\nope\evars.bat",
            r"c:\nope\evars.bat",
            "https://grafana.example",
        ] {
            app.usage.record(key, now);
        }

        press(&mut app, "<Space><Space>");
        assert_eq!(app.mode, Mode::Picker);
        settle(&mut app);
        let names = picker_names(&app);
        assert_eq!(
            names[..5],
            ["E3D HIT", "Grafana", "Work", "E3D HIT", "Grafana"]
        );
        // Then the "+ …" rows, so nothing has to be remembered.
        assert_eq!(names[5], "+ Pin a web app (URL)…");
        assert_eq!(names.len(), 5 + crate::app::launcher::ACTIONS.len());
        let screen = render(&mut app, 160, 30);
        for want in ["Launcher", "frequent", "untagged", "#aveva", "#monitoring"] {
            assert!(screen.contains(want), "{want}: {screen}");
        }
        // Typing finds an action too; Enter on it starts the web pin prompts.
        press(&mut app, "pin a");
        settle(&mut app);
        assert_eq!(
            picker_names(&app)[0],
            "+ Pin a web app (URL)…",
            "a real match ranks first"
        );
        press(&mut app, "<CR>");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::PinName)
        );
        press(&mut app, "<Esc><Esc>");

        // Searching: tiers, no headers.
        press(&mut app, "<Space><Space>wo");
        settle(&mut app);
        assert_eq!(picker_names(&app), ["Work"]);
        assert!(app.picker.as_ref().is_some_and(|p| p.headers.is_empty()));
        // Opening it counts a use.
        press(&mut app, "<CR>");
        assert_eq!(app.cwd.display(), r"C:\work");
        assert!(app.usage.score(r"c:\work", now) > 0);

        // Nothing matches: Enter runs the text (here a program that doesn't exist).
        press(&mut app, "<C-Space>");
        press(&mut app, "sentier-no-such-program-xyz");
        settle(&mut app);
        assert!(picker_names(&app).is_empty());
        press(&mut app, "<CR>");
        assert_eq!(app.mode, Mode::Normal);
    }

    fn wtm_found(app: &mut App, offer: bool) {
        use crate::launcher::entry;
        let Ok((entries, problems)) = entry::parse_config(
            r#"
            [[launcher]]
            name = "E3D HIT"
            command = 'C:\AVEVA\evars.bat'
            args = "HIT"
            group = "AVEVA"
            [[launcher]]
            name = "Grafana"
            command = "msedge.exe"
            args = "--app=https://grafana.example"
            [[launcher]]
            name = "Work"
            command = 'C:\work'
            "#,
        ) else {
            panic!()
        };
        let pairs = entries
            .into_iter()
            .map(|e| {
                let p = entry::to_pin(&e, e.command == r"C:\work");
                (e, p)
            })
            .collect();
        let usage = crate::frecency::Usage::parse("4\t1\tc:\\aveva\\evars.bat hit\n");
        app.handle(AppEvent::WtmFound {
            result: Ok((pairs, usage, problems)),
            offer,
        });
    }

    /// wtm's entries come in as pins after one question (what's pinned already is
    /// skipped), with their launch history; declining the first-run offer is final.
    #[test]
    fn wtm_import_asks_then_adds_new_pins() {
        let (mut app, _rt) = loaded();
        press(&mut app, r":cd C:\work<CR>");
        press(&mut app, "mw<CR><CR>"); // "work" pinned already, as a folder

        wtm_found(&mut app, false);
        let note = app.confirm_note();
        assert!(
            note.contains("Import 2 launcher entries from wtm (1 already pinned)"),
            "{note}"
        );
        press(&mut app, "y");
        let kinds: Vec<(PinKind, &str)> = app
            .pins
            .pins
            .iter()
            .map(|p| (p.kind, p.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (PinKind::Folder, "work"),
                (PinKind::App, "E3D HIT"),
                (PinKind::Web, "Grafana")
            ]
        );
        assert_eq!(app.pins.pins[1].tags, ["aveva"]);
        assert!(
            app.usage.score(r"c:\aveva\evars.bat hit", 1) > 0,
            "history kept"
        );
        // Again: nothing new.
        wtm_found(&mut app, false);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("nothing new"))
        );

        // The first-run offer, declined.
        let (mut app, _rt) = loaded();
        wtm_found(&mut app, true);
        press(&mut app, "n");
        assert!(app.pins.pins.is_empty());
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains(":import-wtm does it any time"))
        );
    }

    /// `:pin url` and `^e` make and edit web and app pins entirely in the app.
    #[test]
    fn web_and_app_pins_are_made_and_edited_in_the_app() {
        let (mut app, _rt) = loaded();
        press(&mut app, ":pin url grafana.example/d/x<CR>");
        assert_eq!(prompt_text(&app).as_deref(), Some("grafana.example"));
        press(&mut app, "<C-u>Grafana<CR>");
        assert_eq!(
            prompt_text(&app).as_deref(),
            Some("https://grafana.example/d/x")
        );
        press(&mut app, "<C-u>not a url<CR>");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("web address"))
        );
        press(&mut app, "<C-u>https://grafana.example/d/x<CR>");
        assert_eq!(prompt_text(&app).as_deref(), Some("edge"));
        press(&mut app, "<C-u>firefox<CR>");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("edge, chrome"))
        );
        press(&mut app, "<C-u>chrome<CR>monitoring<CR>g<CR>");
        assert_eq!(app.mode, Mode::Normal);
        let g = app.pins.pins[0].clone();
        assert_eq!(
            (g.kind, g.url.as_deref(), g.browser.as_deref(), g.key_char()),
            (
                PinKind::Web,
                Some("https://grafana.example/d/x"),
                Some("chrome"),
                Some('g')
            )
        );

        // An app pin, edited with ^e: command, arguments, folder.
        let Ok((store, _)) =
            crate::pins::parse("[[pin]]\nkind='app'\nname='E3D'\ncommand='C:\\a\\e3d.bat'\n")
        else {
            panic!()
        };
        app.pins = store;
        app.edit_pin(0, false, None);
        press(&mut app, "<CR>");
        assert_eq!(prompt_text(&app).as_deref(), Some(r"C:\a\e3d.bat"));
        press(&mut app, "<CR>HIT<CR>");
        press(&mut app, r"D:\aveva<CR><CR><CR>");
        let e = &app.pins.pins[0];
        assert_eq!(
            (e.args.as_deref(), e.dir.as_deref()),
            (Some("HIT"), Some(r"D:\aveva"))
        );
    }

    /// `^s` puts the Start Menu's apps in the launcher; `^a` on one pins it.
    #[test]
    fn launcher_start_menu_apps_and_pinning_them() {
        let (mut app, _rt) = loaded();
        press(&mut app, "<Space><Space>");
        press(&mut app, "<C-s>");
        assert!(app.launcher_start_menu);
        app.handle(AppEvent::StartMenu(vec![(
            "Notepad++".into(),
            r"C:\Start\Notepad++.lnk".into(),
        )]));
        press(&mut app, "note");
        settle(&mut app);
        assert_eq!(picker_names(&app), ["Notepad++"]);
        let screen = render(&mut app, 160, 30);
        assert!(
            screen.contains("[start]") && screen.contains("^s Start Menu"),
            "{screen}"
        );
        press(&mut app, "<C-a>");
        assert_eq!(prompt_text(&app).as_deref(), Some("Notepad++"));
        press(&mut app, "<CR>");
        assert_eq!(
            prompt_text(&app).as_deref(),
            Some(r"C:\Start\Notepad++.lnk")
        );
        press(&mut app, "<CR><CR><CR>tools<CR><CR>");
        assert_eq!(app.pins.pins.len(), 1);
        assert_eq!(app.pins.pins[0].kind, PinKind::App);
        // ^s again: hidden.
        press(&mut app, "<Space><Space><C-s>");
        assert!(!app.launcher_start_menu);
        settle(&mut app);
        assert_eq!(picker_names(&app)[0], "Notepad++");
        assert!(
            app.picker.as_ref().is_some_and(|p| p
                .view
                .iter()
                .all(|i| i.kind != crate::picker::ItemKind::StartMenu)),
            "only the pin now"
        );
    }

    /// pins.toml changed by `sentier pin` (or an editor) is taken in; while a pin is
    /// being edited it waits, so the edit isn't pointed at another pin.
    #[test]
    fn outside_pin_changes_reload_but_wait_for_an_edit() {
        let (mut app, _rt) = loaded();
        let parse = |t: &str| match crate::pins::parse(t) {
            Ok((s, _)) => s,
            Err(e) => panic!("{e}"),
        };
        let one = parse("[[pin]]\nkind='folder'\nname='Work'\npath='C:\\work'\n");
        app.handle(AppEvent::PinsChanged(one.clone()));
        assert_eq!(app.pins, one);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("reloaded"))
        );
        // During an edit: held, then applied when it ends.
        app.edit_pin(0, false, None);
        let two = parse(
            "[[pin]]\nkind='folder'\nname='Work'\npath='C:\\work'\n\
             [[pin]]\nkind='web'\nname='g'\nurl='https://g.example'\n",
        );
        app.handle(AppEvent::PinsChanged(two.clone()));
        assert_eq!(app.pins.pins.len(), 1, "not under an open edit");
        press(&mut app, "<Esc><Esc>");
        app.tick(Instant::now());
        assert_eq!(app.pins, two);
    }

    /// config.toml: hidden files and the gutter follow it; `go` opens the configured
    /// editor on this folder.
    #[test]
    fn config_settings_and_go() {
        let (mut app, _rt) = loaded();
        let Ok(cfg) = crate::config::parse(
            "show_hidden = true\nnumbers = \"absolute\"\neditor = \"code --wait\"\n",
        ) else {
            panic!()
        };
        app.apply_early_config(cfg, None);
        assert!(app.show_hidden);
        assert_eq!((app.number, app.relative_number), (true, false));
        press(&mut app, "go");
        let Some(req) = app.inline.take() else {
            panic!("an inline editor")
        };
        assert_eq!(req.program, "code");
        assert_eq!(req.args, ["--wait", r"C:\work"]);
        // A bad value is said, not fatal.
        let Ok(bad) = crate::config::parse("numbers = \"sometimes\"\n") else {
            panic!()
        };
        app.apply_early_config(bad, Some("config.toml: slow".into()));
        assert!(app.status.as_ref().is_some_and(|s| s.is_error));
    }
}

#[cfg(test)]
mod preview_tests {
    //! The picture preview without a terminal: the worker's answer is injected (the test
    //! runtime is never driven), the frame is drawn into a TestBackend.
    use super::tests::{entry, fixture, listing, press, render};
    use super::*;
    use crate::preview::image::load;

    /// The fixture's C:\work plus `photo.png`, with the picture loaded from a temp file.
    fn with_photo() -> (
        App,
        tokio::runtime::Runtime,
        Arc<crate::preview::image::ImagePreview>,
    ) {
        let (mut app, rt) = fixture();
        app.graphics = ratatui_image::picker::Picker::halfblocks();
        let cwd = app.cwd.clone();
        app.dirs.insert(
            cwd.clone(),
            DirState {
                listing: Some(listing(vec![
                    entry("notes.txt", EntryKind::File, 1536, false),
                    entry("photo.png", EntryKind::File, 4096, false),
                ])),
                ..DirState::default()
            },
        );
        app.set_cwd(cwd, None);
        // One folder per call: tests run in parallel and each removes its own.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sentier-prev-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("photo.png");
        let img = image::RgbaImage::from_pixel(400, 200, image::Rgba([200, 30, 30, 255]));
        assert!(img.save(&path).is_ok());
        let Ok(p) = load(&path.to_string_lossy(), &app.graphics, 30, 20) else {
            panic!("load")
        };
        let _ = std::fs::remove_dir_all(&dir);
        (app, rt, Arc::new(p))
    }

    /// Lets the cursor settle, so the picture is asked for.
    fn settle(app: &mut App) {
        app.tick(Instant::now() + preview::IMAGE_STILL + Duration::from_millis(1));
    }

    #[test]
    fn picture_waits_for_the_cursor_then_draws_and_is_cached() {
        let (mut app, _rt, picture) = with_photo();
        render(&mut app, 120, 30); // the UI reports the picture box
        press(&mut app, "j"); // photo.png
        app.tick(Instant::now());
        assert!(app.image.is_none(), "not while the cursor is moving");
        settle(&mut app);
        let Some(key) = app.image.as_ref().map(|s| s.key.clone()) else {
            panic!("asked for")
        };
        assert_eq!(key.loc.display(), r"C:\work\photo.png");
        assert!(app.is_busy(), "spinner while it's read");
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("rendering"), "{screen}");

        // A late answer for another file is ignored…
        let mut other = key.clone();
        other.cols += 1;
        app.handle(AppEvent::ImagePreview {
            key: other,
            result: Err("stale".into()),
        });
        assert!(app.image.as_ref().is_some_and(|s| s.result.is_none()));
        // …the right one is drawn.
        app.handle(AppEvent::ImagePreview {
            key: key.clone(),
            result: Ok(Arc::clone(&picture)),
        });
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("400×200 · PNG"), "{screen}");
        assert!(app.image_drawn.is_some(), "the UI says where it drew");

        // Another file: no picture drawn (so the loop erases the old one).
        press(&mut app, "k");
        render(&mut app, 120, 30);
        assert!(app.image_drawn.is_none());

        // Back: straight from the cache, no worker round trip.
        press(&mut app, "j");
        settle(&mut app);
        assert!(
            app.image.as_ref().is_some_and(|s| s.result.is_some()),
            "cached"
        );
        render(&mut app, 120, 30);
        assert!(app.image_drawn.is_some());

        // A help overlay covers the column: no picture under it.
        press(&mut app, "g?");
        render(&mut app, 120, 30);
        assert!(app.image_drawn.is_none());
    }

    #[test]
    fn errors_show_in_the_panel() {
        let (mut app, _rt, _) = with_photo();
        render(&mut app, 120, 30);
        press(&mut app, "j");
        settle(&mut app);
        let Some(key) = app.image.as_ref().map(|s| s.key.clone()) else {
            panic!()
        };
        app.handle(AppEvent::ImagePreview {
            key,
            result: Err("large file on the network (80 MB): not previewed".into()),
        });
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("large file on the network"), "{screen}");
        assert!(app.image_drawn.is_none());
    }

    #[test]
    fn iterm2_pictures_are_placed_after_the_frame_not_drawn_in_it() {
        let (mut app, _rt, _) = with_photo();
        let mut iterm2 = ratatui_image::picker::Picker::halfblocks();
        iterm2.set_protocol_type(ratatui_image::picker::ProtocolType::Iterm2);
        app.graphics = iterm2;
        render(&mut app, 120, 30);
        press(&mut app, "j");
        settle(&mut app);
        let Some(key) = app.image.as_ref().map(|s| s.key.clone()) else {
            panic!()
        };
        let dir = std::env::temp_dir().join(format!("sentier-prev2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("photo.png");
        assert!(
            image::RgbaImage::from_pixel(64, 32, image::Rgba([1, 2, 3, 255]))
                .save(&path)
                .is_ok()
        );
        let Ok(p) = load(&path.to_string_lossy(), &app.graphics, key.cols, key.rows) else {
            panic!()
        };
        let _ = std::fs::remove_dir_all(&dir);
        app.handle(AppEvent::ImagePreview {
            key,
            result: Ok(Arc::new(p)),
        });
        let screen = render(&mut app, 120, 30);
        assert!(app.image_drawn.is_some());
        assert!(app.image_place.is_some(), "handed to the UI loop");
        assert!(!screen.contains("1337"), "not in ratatui's buffer");

        // The which-key popup would be covered by a picture written after the frame.
        press(&mut app, "g");
        app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
        render(&mut app, 120, 30);
        assert!(app.image_drawn.is_none() && app.image_place.is_none());
    }

    #[test]
    fn pdf_pages_step_clamp_and_are_remembered() {
        let status = |app: &App| {
            app.status
                .as_ref()
                .map(|s| s.text.clone())
                .unwrap_or_default()
        };
        let (mut app, _rt, _) = with_photo();
        let cwd = app.cwd.clone();
        app.dirs.insert(
            cwd.clone(),
            DirState {
                listing: Some(listing(vec![
                    entry("notes.txt", EntryKind::File, 1536, false),
                    entry("plan.pdf", EntryKind::File, 9000, false),
                ])),
                ..DirState::default()
            },
        );
        app.set_cwd(cwd, None);
        render(&mut app, 120, 30);
        press(&mut app, "j"); // plan.pdf
        settle(&mut app);
        let page = |app: &App| app.image.as_ref().map(|s| s.key.page);
        assert_eq!(page(&app), Some(0));
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("rendering page 1"), "{screen}");

        // Two pages: the answer tells the count.
        let pdf = crate::platform::tests::tiny_pdf(&[(595, 842), (1191, 842)]);
        let Some(key) = app.image.as_ref().map(|s| s.key.clone()) else {
            panic!()
        };
        let Ok(p) =
            crate::preview::image::pdf(&pdf, 0, &app.graphics, key.cols, key.rows, key.view)
        else {
            panic!()
        };
        app.handle(AppEvent::ImagePreview {
            key,
            result: Ok(Arc::new(p)),
        });
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("page 1 / 2 · A4 portrait"), "{screen}");

        press(&mut app, "5]p");
        app.tick(Instant::now());
        assert_eq!(page(&app), Some(1), "clamped to the last page");
        press(&mut app, "[p");
        app.tick(Instant::now());
        assert_eq!(page(&app), Some(0));
        press(&mut app, "[p");
        assert!(status(&app).contains("first page"));
        press(&mut app, "2gP");
        app.tick(Instant::now());
        assert_eq!(page(&app), Some(1));
        press(&mut app, "gP1<CR>");
        app.tick(Instant::now());
        assert_eq!(page(&app), Some(0));
        press(&mut app, "gPx<CR>");
        assert!(status(&app).contains("not a page number"));

        // Leave and come back: the page is kept.
        press(&mut app, "2gPkj");
        settle(&mut app);
        assert_eq!(page(&app), Some(1));

        // Not on a PDF.
        press(&mut app, "k]p");
        assert!(status(&app).contains("PDF under the cursor"));
    }

    #[test]
    fn thumbnails_fall_back_to_file_info() {
        let (mut app, _rt, _) = with_photo();
        let cwd = app.cwd.clone();
        app.dirs.insert(
            cwd.clone(),
            DirState {
                listing: Some(listing(vec![
                    entry("notes.txt", EntryKind::File, 1536, false),
                    entry("spec.docx", EntryKind::File, 20480, false),
                ])),
                ..DirState::default()
            },
        );
        app.set_cwd(cwd, None);
        render(&mut app, 120, 30);
        press(&mut app, "j"); // spec.docx
        settle(&mut app);
        let Some(key) = app.image.as_ref().map(|s| s.key.clone()) else {
            panic!("a thumbnail is asked for")
        };
        // While Windows is asked, and when it has none: the file info, no spinner.
        let screen = render(&mut app, 120, 30);
        assert!(
            screen.contains("Size") && !screen.contains("rendering"),
            "{screen}"
        );
        app.handle(AppEvent::ImagePreview {
            key: key.clone(),
            result: Err("no Windows thumbnail".into()),
        });
        let screen = render(&mut app, 120, 30);
        assert!(
            screen.contains("Size") && !screen.contains("no Windows thumbnail"),
            "{screen}"
        );
        assert!(app.image_drawn.is_none());

        // With one: the picture and its caption.
        let mut png = Vec::new();
        assert!(
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                80,
                60,
                image::Rgba([9, 9, 9, 255])
            ))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .is_ok()
        );
        let Ok(mut thumb) =
            crate::preview::image::decode(&png, &app.graphics, key.cols, key.rows, key.view)
        else {
            panic!()
        };
        thumb.thumbnail = true;
        app.image = None;
        app.image_cache = crate::preview::Lru::new(4);
        settle(&mut app);
        app.handle(AppEvent::ImagePreview {
            key,
            result: Ok(Arc::new(thumb)),
        });
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("Windows thumbnail · 20.0 KB"), "{screen}");
        assert!(app.image_drawn.is_some());

        // Text files aren't even asked about.
        press(&mut app, "k");
        settle(&mut app);
        assert!(
            app.image
                .as_ref()
                .is_some_and(|s| s.key.loc.name() == "spec.docx")
        );
    }

    /// C:\work holding `files`, the cursor on the first, the picture box reported.
    fn gallery(files: &[&str]) -> (App, tokio::runtime::Runtime) {
        let (mut app, rt, _) = with_photo();
        let cwd = app.cwd.clone();
        let entries = files
            .iter()
            .map(|n| entry(n, EntryKind::File, 4096, false))
            .collect();
        app.dirs.insert(
            cwd.clone(),
            DirState {
                listing: Some(listing(entries)),
                ..DirState::default()
            },
        );
        app.set_cwd(cwd, None);
        app.set_cursor(0);
        render(&mut app, 120, 30);
        (app, rt)
    }

    fn answer(app: &mut App, key: crate::preview::ImageKey) {
        let mut png = Vec::new();
        assert!(
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                64,
                64,
                image::Rgba([1, 1, 1, 255])
            ))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .is_ok()
        );
        let Ok(p) =
            crate::preview::image::decode(&png, &app.graphics, key.cols, key.rows, key.view)
        else {
            panic!()
        };
        app.handle(AppEvent::ImagePreview {
            key,
            result: Ok(Arc::new(p)),
        });
    }

    fn current(app: &App) -> Option<crate::preview::ImageKey> {
        app.image.as_ref().map(|s| s.key.clone())
    }

    #[test]
    fn preloads_neighbours_once_and_uses_them() {
        let (mut app, _rt) = gallery(&["a.png", "b.png", "c.png", "notes.txt"]);
        press(&mut app, "j"); // b.png
        settle(&mut app);
        let Some(b) = current(&app) else { panic!() };
        assert_eq!(app.image_inflight.len(), 1, "only the current one so far");
        answer(&mut app, b);
        let names: Vec<String> = app.image_inflight.iter().map(|k| k.loc.name()).collect();
        assert_eq!(app.image_inflight.len(), 2, "{names:?}");
        assert!(names.contains(&"a.png".into()) && names.contains(&"c.png".into()));

        // Onto c.png while its preload is on the way: not asked twice.
        press(&mut app, "j");
        settle(&mut app);
        assert_eq!(app.image_inflight.len(), 2, "no duplicate request");
        let Some(c) = current(&app) else { panic!() };
        answer(&mut app, c);
        assert!(
            app.image
                .as_ref()
                .is_some_and(|s| matches!(s.result, Some(Ok(_))))
        );
        // c's neighbours: b is cached, notes.txt has nothing to preview.
        let names: Vec<String> = app.image_inflight.iter().map(|k| k.loc.name()).collect();
        assert_eq!(names, ["a.png"], "only a.png is still on its way");
    }

    #[test]
    fn missing_thumbnails_are_not_asked_again() {
        let (mut app, _rt) = gallery(&["notes.txt", "spec.docx"]);
        press(&mut app, "j");
        settle(&mut app);
        let Some(key) = current(&app) else { panic!() };
        app.handle(AppEvent::ImagePreview {
            key,
            result: Err("no Windows thumbnail".into()),
        });
        press(&mut app, "kj");
        settle(&mut app);
        assert!(app.image_inflight.is_empty(), "remembered, not re-asked");
        assert!(
            app.image
                .as_ref()
                .is_some_and(|s| matches!(s.result, Some(Err(_))))
        );
    }

    #[test]
    fn full_screen_zoom_pan_and_next_file() {
        let (mut app, _rt) = gallery(&["a.txt", "b.png", "c.png"]);
        press(&mut app, "zp");
        assert_eq!(app.mode, Mode::Normal, "nothing to show for a .txt");
        let small = app.image_area.map(|r| r.width).unwrap_or(0);
        press(&mut app, "jzp");
        assert_eq!(app.mode, Mode::Preview);
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("PREVIEW"), "{screen}");
        let big = app.image_area.map(|r| r.width).unwrap_or(0);
        assert!(big > small * 2, "the whole body: {small} → {big}");
        settle(&mut app);
        assert_eq!(
            current(&app).map(|k| (k.loc.name(), k.cols)),
            Some(("b.png".into(), big))
        );

        press(&mut app, "++L");
        settle(&mut app);
        let Some(k) = current(&app) else { panic!() };
        assert_eq!(k.view.zoom, 200);
        assert!(k.view.cx > 500, "panned right");
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("b.png · 200 %"), "{screen}");
        press(&mut app, "=");
        settle(&mut app);
        assert_eq!(current(&app).map(|k| k.view.zoom), Some(100));

        // j: the next file, zoom starts over.
        press(&mut app, "+j");
        settle(&mut app);
        let Some(k) = current(&app) else { panic!() };
        assert_eq!((k.loc.name().as_str(), k.view.zoom), ("c.png", 100));

        press(&mut app, "zp");
        assert_eq!(app.mode, Mode::Normal);
        press(&mut app, "zp<Esc>");
        assert_eq!(app.mode, Mode::Normal, "Esc leaves too");
    }
}

#[cfg(test)]
mod auth_tests {
    //! The credential flow without a server: listing errors and connection results are
    //! injected as events (the test runtime is never driven, so nothing really connects).
    use super::tests::{fixture, press, render};
    use super::*;
    use crate::platform::NetConnection;

    fn loc(s: &str) -> Loc {
        let Ok(l) = Loc::parse(s) else { panic!("{s}") };
        l
    }

    fn status(app: &App) -> String {
        app.status
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default()
    }

    /// The app in `path`, whose listing then fails with `err`.
    fn refused(path: &str, err: ListError) -> (App, tokio::runtime::Runtime) {
        let (mut app, rt) = fixture();
        let target = loc(path);
        app.navigate(target.clone(), None);
        let Some(id) = app.pending.get(&target).map(|r| r.id) else {
            panic!("listing requested")
        };
        app.handle(AppEvent::Listed {
            id,
            loc: target,
            result: Err(err),
        });
        (app, rt)
    }

    fn connected(app: &mut App, requested: &str, target: &str, result: Result<(), u32>) {
        app.handle(AppEvent::Connected {
            requested: requested.to_string(),
            target: target.to_string(),
            user: r"CORP\me".to_string(),
            result,
            remembered: None,
            dropped: Vec::new(),
        });
    }

    #[test]
    fn refused_share_asks_once_and_masks_the_password() {
        let (mut app, _rt) = refused(r"\\srv01\project\HIT", ListError::LogonFailure(1326));
        assert_eq!(app.mode, Mode::Credentials);
        let Some(p) = app.cred.as_ref() else { panic!() };
        assert_eq!(p.target, r"\\srv01\project");
        assert!(p.reason.contains("logon failure") && p.reason.contains("1326"));

        press(&mut app, r"CORP\me<Tab>hunter2");
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("Credentials needed"), "{screen}");
        assert!(screen.contains(r"CORP\me"));
        assert!(screen.contains("●●●●●●●"), "{screen}");
        assert!(!screen.contains("hunter2"), "the password is never drawn");

        press(&mut app, "<Esc>");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.cred.is_none(), "dropped (and wiped)");
        // The same server refusing again doesn't pop it up again…
        app.refresh();
        let target = app.cwd.clone();
        if let Some(id) = app.pending.get(&target).map(|r| r.id) {
            app.handle(AppEvent::Listed {
                id,
                loc: target,
                result: Err(ListError::AccessDenied),
            });
        }
        assert_eq!(app.mode, Mode::Normal, "asked once per server");
        // …but :connect does.
        press(&mut app, ":connect<CR>");
        assert_eq!(app.mode, Mode::Credentials);
    }

    #[test]
    fn wrong_password_retries_then_success_closes() {
        let (mut app, _rt) = refused(r"\\srv01\project", ListError::AccessDenied);
        press(&mut app, "<CR>");
        assert!(
            app.cred
                .as_ref()
                .is_some_and(|p| p.reason.contains("user name"))
        );
        press(&mut app, r"CORP\me<CR>");
        assert!(
            app.cred
                .as_ref()
                .is_some_and(|p| p.reason.contains("password"))
        );
        press(&mut app, "secret<CR>");
        assert!(app.cred.as_ref().is_some_and(|p| p.busy));

        connected(&mut app, r"\\srv01\project", r"\\srv01\project", Err(1326));
        let Some(p) = app.cred.as_ref() else { panic!() };
        assert!(!p.busy && p.reason.contains("try 2 of 3"), "{}", p.reason);
        assert_eq!(p.password_len(), 0, "cleared for the next try");

        press(&mut app, "right<CR>");
        connected(&mut app, r"\\srv01\project", r"\\srv01\project", Ok(()));
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            status(&app).contains(r"connected to \\srv01\project as CORP\me"),
            "{}",
            status(&app)
        );
        assert!(app.pending.contains_key(&app.cwd), "listed again");
    }

    #[test]
    fn error_1219_offers_disconnect_or_alias() {
        let (mut app, _rt) = refused(r"\\srv01\project\HIT", ListError::AccessDenied);
        press(&mut app, r"CORP\me<Tab>pw<CR>");
        connected(&mut app, r"\\srv01\project", r"\\srv01\project", Err(1219));
        let Some(p) = app.cred.as_ref() else { panic!() };
        assert!(p.conflict && p.password_len() == 2, "kept for the fix");
        let screen = render(&mut app, 120, 30);
        assert!(
            screen.contains("disconnect the existing connection"),
            "{screen}"
        );
        assert!(screen.contains("IP address"), "{screen}");

        // a: through the alias, then the same folder on it.
        press(&mut app, "a");
        assert!(app.cred.as_ref().is_some_and(|p| p.busy));
        connected(&mut app, r"\\srv01\project", r"\\10.0.0.5\project", Ok(()));
        assert_eq!(app.cwd.display(), r"\\10.0.0.5\project\HIT");
    }

    #[test]
    fn admin_shares_prefill_the_admin_profile() {
        let (mut app, _rt) = fixture();
        if let Ok(c) = crate::config::parse("[auth.admin]\nuser = 'CORP\\adm-user'\n") {
            app.config = c;
        }
        let target = loc(r"\\PC-042\C$\Users");
        app.navigate(target.clone(), None);
        let Some(id) = app.pending.get(&target).map(|r| r.id) else {
            panic!()
        };
        app.handle(AppEvent::Listed {
            id,
            loc: target,
            result: Err(ListError::AccessDenied),
        });
        let Some(p) = app.cred.as_ref() else { panic!() };
        assert_eq!(p.user.text(), r"CORP\adm-user");
        assert_eq!(p.profile.as_deref(), Some("admin"));
        assert_eq!(p.focus, auth::Field::Password);
    }

    #[test]
    fn net_list_and_drop() {
        let (mut app, _rt) = fixture();
        let conns = vec![
            NetConnection {
                remote: r"\\srv01\project".into(),
                local: None,
                user: Some(r"CORP\me".into()),
            },
            NetConnection {
                remote: r"\\srv01\aveva".into(),
                local: Some("P:".into()),
                user: None,
            },
        ];
        press(&mut app, ":net list<CR>");
        app.handle(AppEvent::NetList(Ok(conns.clone())));
        assert_eq!(app.mode, Mode::NetList);
        let screen = render(&mut app, 120, 30);
        assert!(
            screen.contains(r"\\srv01\project") && screen.contains("P:"),
            "{screen}"
        );
        assert!(screen.contains("(current login)"));
        press(&mut app, "<Esc>");
        assert_eq!(app.mode, Mode::Normal);

        // drop: only the deviceless one is asked to go; P: stays.
        press(&mut app, ":net drop SRV01<CR>");
        app.handle(AppEvent::NetList(Ok(conns)));
        assert_eq!(app.mode, Mode::Normal, "no list shown for a drop");
        app.handle(AppEvent::NetDropped(vec![(
            r"\\srv01\project".into(),
            Ok(()),
        )]));
        assert!(status(&app).contains(r"disconnected \\srv01\project"));

        press(&mut app, ":net drop-all<CR>");
        assert!(status(&app).contains("hasn't made any"));
        press(&mut app, ":net frob<CR>");
        assert!(app.status.as_ref().is_some_and(|s| s.is_error));
    }
}

#[cfg(test)]
mod machine_tests {
    //! Machines without a network: probe and user lookups are injected as events (the
    //! test runtime is never driven).
    use super::tests::{fixture, press, render};
    use super::*;
    use crate::fs::worker::MachineUsers;
    use crate::machines::Recent;

    fn status(app: &App) -> String {
        app.status
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default()
    }

    fn with_recent(hosts: &[&str]) -> (App, tokio::runtime::Runtime) {
        let (mut app, rt) = fixture();
        app.on_pins_loaded(Ok((crate::pins::PinStore::default(), Vec::new())));
        let now = SystemTime::now();
        app.handle(AppEvent::RecentMachines(Ok(hosts
            .iter()
            .map(|h| Recent {
                host: h.to_string(),
                opened: now,
                seen: None,
            })
            .collect())));
        (app, rt)
    }

    fn up(app: &mut App, host: &str, logged_on: &[&str]) {
        app.handle(AppEvent::MachineProbe {
            host: host.to_string(),
            result: Ok(Duration::from_millis(4)),
        });
        app.handle(AppEvent::MachineUsers {
            host: host.to_string(),
            result: Ok(MachineUsers {
                logged_on: logged_on.iter().map(|s| s.to_string()).collect(),
                profiles: vec!["jdoe".into(), "asmith".into()],
                likely: Some("jdoe".into()),
            }),
        });
    }

    fn names(app: &App) -> Vec<String> {
        app.view_listing
            .as_ref()
            .map(|l| {
                app.view
                    .iter()
                    .map(|&i| l.entries[i].name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn machines_list_view_templates_and_keys() {
        let (mut app, _rt) = with_recent(&["PC-042", "PC-007"]);
        press(&mut app, "gm");
        assert_eq!(app.cwd, Loc::Machines);
        assert_eq!(names(&app), ["PC-042", "PC-007"]);

        press(&mut app, "l"); // PC-042
        assert_eq!(app.cwd, Loc::Machine("PC-042".into()));
        // Before the users are known, templates with {user} wait for one.
        press(&mut app, "d");
        assert!(status(&app).contains("who's the user"), "{}", status(&app));

        up(&mut app, "PC-042", &["jdoe"]);
        let n = names(&app);
        assert_eq!(n[0], r"\\PC-042\C$\Users\jdoe\Desktop");
        assert_eq!(n.last().map(String::as_str), Some(r"\\PC-042\C$"));
        app.status = None; // the "who's the user" hint above would hide the machine line
        let screen = render(&mut app, 140, 30);
        assert!(
            screen.contains("ADMIN SHARE") && screen.contains("PC-042  ·  jdoe"),
            "{screen}"
        );
        assert!(screen.contains("d  Desktop"), "{screen}");
        assert!(screen.contains("online · 4 ms"), "{screen}");

        // d jumps to jdoe's Desktop.
        press(&mut app, "d");
        assert_eq!(app.cwd.display(), r"\\PC-042\C$\Users\jdoe\Desktop");
        // From the C$ root, h goes back to the machine view.
        press(&mut app, r":cd \\PC-042\C$<CR>");
        press(&mut app, "h");
        assert_eq!(app.cwd, Loc::Machine("PC-042".into()));

        // u: templates for someone else.
        press(&mut app, "u<C-u>asmith<CR>");
        assert_eq!(names(&app)[0], r"\\PC-042\C$\Users\asmith\Desktop");
        assert!(status(&app).contains("templates for asmith"));
    }

    #[test]
    fn offline_machines_fail_fast() {
        let (mut app, _rt) = with_recent(&[]);
        let target = Loc::parse(r"\\PC-OFF\C$\Users").unwrap_or(Loc::Root);
        app.navigate(target.clone(), None);
        assert!(app.pending.contains_key(&target), "listing asked");
        app.handle(AppEvent::MachineProbe {
            host: "PC-OFF".into(),
            result: Err("port 445 doesn't answer".into()),
        });
        assert!(!app.pending.contains_key(&target), "no waiting on SMB");
        let err = app.dir(&target).and_then(|s| s.error.clone());
        let text = err.map(|e| e.to_string()).unwrap_or_default();
        assert!(
            text.contains("offline") && text.contains("never seen"),
            "{text}"
        );
        // And it's remembered as a recent machine.
        assert_eq!(
            app.recent_machines.first().map(|r| r.host.as_str()),
            Some("PC-OFF")
        );
    }

    #[test]
    fn pin_a_machine_and_jump_back() {
        let (mut app, _rt) = with_recent(&["PC-042"]);
        press(&mut app, "gm");
        press(&mut app, "M");
        assert_eq!(
            app.prompt.as_ref().map(|p| p.editor.text()).as_deref(),
            Some("PC-042")
        );
        press(&mut app, "<CR>users laptop<CR>j<CR>");
        let pin = &app.pins.pins[0];
        assert_eq!(pin.kind, crate::pins::PinKind::Machine);
        assert_eq!(
            pin.extra.get("host").and_then(|v| v.as_str()),
            Some("PC-042")
        );
        assert_eq!(
            pin.extra.get("auth").and_then(|v| v.as_str()),
            Some("admin")
        );
        assert_eq!(pin.key_char(), Some('j'));

        press(&mut app, "gl'j");
        assert_eq!(app.cwd, Loc::Machine("PC-042".into()));
        // Pinned machines come first in the list, and once.
        press(&mut app, "gm");
        assert_eq!(names(&app), ["PC-042"]);
    }

    #[test]
    fn locations_have_a_machines_section() {
        use crate::platform::DriveKind;
        let (mut app, _rt) = with_recent(&["PC-042"]);
        let mut entries = vec![Entry::virtual_entry(
            "C:".into(),
            EntryKind::Drive(DriveKind::Fixed),
            "Windows".into(),
        )];
        entries.extend(app.machine_root_entries());
        app.dirs.insert(
            Loc::Root,
            DirState {
                listing: Some(super::tests::listing_unsorted(entries)),
                ..DirState::default()
            },
        );
        app.set_cwd(Loc::Root, None);
        app.pending.clear();
        let screen = render(&mut app, 140, 30);
        assert!(
            screen.contains("MACHINES") && screen.contains("PC-042"),
            "{screen}"
        );
        press(&mut app, "jl");
        assert_eq!(app.cwd, Loc::Machine("PC-042".into()));
    }

    // ------------------------------------------------------------ machine picker

    fn settle(app: &mut App) {
        let end = Instant::now() + Duration::from_secs(5);
        for _ in 0..3 {
            app.tick(Instant::now());
        }
        while app.picker.as_ref().is_some_and(|p| p.running) && Instant::now() < end {
            app.tick(Instant::now());
        }
    }

    fn picker_names(app: &App) -> Vec<String> {
        app.picker
            .as_ref()
            .map(|p| p.view.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default()
    }

    fn record(host: &str, user: &str, details: &[&str]) -> crate::machines::MachineRecord {
        crate::machines::MachineRecord {
            host: host.into(),
            user: Some(user.into()),
            details: details.iter().map(|s| s.to_string()).collect(),
            source: "inventory",
        }
    }

    fn query(app: &mut App, text: &str) {
        press(app, "<C-u>");
        press(app, text);
        settle(app);
    }

    #[test]
    fn machine_picker_lists_pinned_recent_and_finds_people() {
        let (mut app, _rt) = with_recent(&["PC-042", "PC-007"]);
        // Pin PC-007 from its machine view.
        press(&mut app, "gmjM<CR>laptop<CR><CR>");
        assert_eq!(app.pins.pins.len(), 1);
        press(&mut app, "<leader>m");
        settle(&mut app);
        assert_eq!(app.picker.as_ref().map(|p| p.title), Some("Machines"));
        let mut names = picker_names(&app);
        names.retain(|n| !n.starts_with('+'));
        names.sort();
        assert_eq!(names, ["PC-007", "PC-042"], "pinned once, recent once");

        // The inventory arrives while the picker is open: new hosts are added, and the
        // users and details become searchable.
        app.handle(AppEvent::MachineSources(vec![(
            "inventory",
            Ok(vec![
                record("pc-042", "jdoe", &["Piping"]),
                record("PC-100", "asmith", &["laptop", "Structural"]),
            ]),
        )]));
        settle(&mut app);
        assert_eq!(picker_names(&app).len(), 4, "3 machines and the add row");
        query(&mut app, "asmith");
        assert_eq!(picker_names(&app), ["PC-100"]);
        let screen = render(&mut app, 140, 30);
        assert!(
            screen.contains("asmith") && screen.contains("inventory"),
            "{screen}"
        );
        query(&mut app, "struct");
        assert_eq!(picker_names(&app), ["PC-100"]);

        // Reopened: rows already known carry what the inventory says.
        press(&mut app, "<Esc><Esc>");
        press(&mut app, "<leader>m");
        query(&mut app, "jdoe");
        assert_eq!(picker_names(&app), ["PC-042"]);
    }

    #[test]
    fn machine_picker_filters_by_probe() {
        let (mut app, _rt) = with_recent(&["PC-042", "PC-007"]);
        app.handle(AppEvent::MachineProbe {
            host: "PC-042".into(),
            result: Ok(Duration::from_millis(3)),
        });
        app.handle(AppEvent::MachineProbe {
            host: "PC-007".into(),
            result: Err("no answer on port 445".into()),
        });
        press(&mut app, "<leader>m");
        query(&mut app, "is:online ");
        assert_eq!(picker_names(&app), ["PC-042"]);
        query(&mut app, "is:offline ");
        assert_eq!(picker_names(&app), ["PC-007"]);
        query(&mut app, "is:machine ");
        assert_eq!(picker_names(&app).len(), 2);
        // The pin picker knows machines too.
        press(&mut app, "<Esc><Esc>");
        press(&mut app, "gmM<CR><CR><CR>");
        press(&mut app, "<leader>p");
        query(&mut app, "is:machine ");
        assert_eq!(picker_names(&app).len(), 1);
    }

    #[test]
    fn machine_picker_opens_and_pins() {
        let (mut app, _rt) = with_recent(&["PC-042"]);
        app.handle(AppEvent::MachineSources(vec![(
            "inventory",
            Ok(vec![record("PC-100", "asmith", &[])]),
        )]));
        press(&mut app, "<leader>m");
        query(&mut app, "pc-100");
        press(&mut app, "<CR>");
        assert_eq!(app.cwd, Loc::Machine("PC-100".into()));
        assert!(app.picker.is_none());

        press(&mut app, "<leader>m");
        query(&mut app, "pc-100");
        press(&mut app, "<C-x>");
        assert!(status(&app).contains("isn't pinned"), "{}", status(&app));
        press(&mut app, "<C-a>");
        assert!(app.picker.is_none());
        press(&mut app, "<CR>users<CR>p<CR>");
        let pin = &app.pins.pins[0];
        assert_eq!(
            pin.extra.get("host").and_then(|v| v.as_str()),
            Some("PC-100")
        );
        assert_eq!(pin.key_char(), Some('p'));
    }

    /// A machine never met: `a` in `gm` asks for its name; `Space m` + a name with no
    /// match opens it; the "+ Add a machine…" row asks too. Each opens its machine view
    /// (probed) and makes it a recent machine; a bad name keeps the prompt with why.
    #[test]
    fn machines_are_added_by_name() {
        let (mut app, _rt) = with_recent(&["PC-042"]);
        // gm, a: the name prompt; a bad name stays and says why.
        press(&mut app, "gm");
        let screen = render(&mut app, 140, 30);
        assert!(screen.contains("add a machine"), "the hint row: {screen}");
        press(&mut app, "a");
        assert_eq!(
            app.prompt.as_ref().map(|p| p.kind),
            Some(PromptKind::MachineName)
        );
        press(&mut app, "bad name");
        press(&mut app, "<CR>");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("isn't a PC name")),
            "stays open with why"
        );
        press(&mut app, "<C-u>");
        press(&mut app, r"\\PC-555\C$");
        press(&mut app, "<CR>");
        assert_eq!(app.cwd, Loc::Machine("PC-555".into()));
        assert!(app.recent_machines.iter().any(|r| r.host == "PC-555"));
        assert!(
            status(&app).contains("M or m + a key pins it"),
            "{}",
            status(&app)
        );

        // Space m, a name nothing matches, Enter: opened as a new machine.
        press(&mut app, "<leader>m");
        settle(&mut app);
        query(&mut app, "PC-777");
        let screen = render(&mut app, 140, 30);
        assert!(screen.contains("Enter opens it as a new one"), "{screen}");
        press(&mut app, "<CR>");
        assert_eq!(app.cwd, Loc::Machine("PC-777".into()));

        // The row: asks for the name.
        press(&mut app, "<leader>m");
        settle(&mut app);
        query(&mut app, "add a machine");
        assert_eq!(picker_names(&app), ["+ Add a machine…"]);
        press(&mut app, "<CR>");
        assert_eq!(
            app.prompt.as_ref().map(|p| p.kind),
            Some(PromptKind::MachineName)
        );
        press(&mut app, "<Esc><Esc>");
        assert!(
            status(&app).contains("no machine opened"),
            "{}",
            status(&app)
        );
    }

    #[test]
    fn machine_source_errors_show() {
        let (mut app, _rt) = with_recent(&[]);
        press(&mut app, "<leader>m");
        settle(&mut app);
        let screen = render(&mut app, 140, 30);
        assert!(
            screen.contains("+ Add a machine…"),
            "no machines yet: the way to add one, {screen}"
        );
        app.handle(AppEvent::MachineSources(vec![(
            "command",
            Err("source_cmd: timed out after 20 s".into()),
        )]));
        let screen = render(&mut app, 140, 30);
        assert!(screen.contains("timed out after 20 s"), "{screen}");
    }
}

#[cfg(test)]
mod job_tests {
    use super::tests::{fixture, press, render};
    use super::*;
    use crate::fs::worker::Timeouts;
    use crate::jobs::robocopy::{Event as RcEvent, FileClass};
    use crate::jobs::{JobEvent, Phase};
    use ratatui::crossterm::event::KeyEvent;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sentier-jobs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("src").join("proj").join("sub"));
        let _ = std::fs::create_dir_all(dir.join("dst"));
        let _ = std::fs::write(dir.join("src").join("a.txt"), "a");
        let _ = std::fs::write(dir.join("src").join("b.txt"), "bb");
        let _ = std::fs::write(dir.join("src").join("proj").join("sub").join("c.txt"), "c");
        dir
    }

    /// An app with a real worker and channel, started in `dir`.
    struct Live {
        app: App,
        rx: crate::event::EventRx,
        _rt: tokio::runtime::Runtime,
    }

    impl Live {
        fn new(dir: &Path) -> Live {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            else {
                panic!("runtime")
            };
            let (tx, rx) = crate::event::channel();
            let worker = Worker::new(rt.handle().clone(), tx, Timeouts::default());
            let Ok(cwd) = Loc::parse(&dir.display().to_string()) else {
                panic!()
            };
            let app = App::new(worker, cwd);
            let mut live = Live { app, rx, _rt: rt };
            live.until(|a| a.view_listing.is_some() && a.pending.is_empty());
            live
        }

        /// Handles events until `done` holds (5 s at most).
        fn until(&mut self, done: impl Fn(&App) -> bool) {
            let end = Instant::now() + Duration::from_secs(10);
            while !done(&self.app) {
                assert!(
                    Instant::now() < end,
                    "timed out; status {:?}, view {:?}, cursor on {:?}, ops {}",
                    self.app.status.as_ref().map(|s| s.text.clone()),
                    self.names(),
                    self.app.selected().map(|e| e.name.clone()),
                    self.app.ops_running
                );
                if let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(20)) {
                    self.app.handle(ev);
                }
                self.app.tick(Instant::now());
            }
        }

        fn cd(&mut self, path: &Path) {
            press(&mut self.app, ":");
            for c in format!("cd {}", path.display()).chars() {
                self.app.handle(AppEvent::Term(Event::Key(KeyEvent::new(
                    crossterm_char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                ))));
            }
            press(&mut self.app, "<CR>");
            let want = path.display().to_string().to_lowercase();
            self.until(|a| a.cwd.display().to_lowercase() == want && a.view_listing.is_some());
        }

        fn names(&self) -> Vec<String> {
            let Some(l) = &self.app.view_listing else {
                return Vec::new();
            };
            self.app
                .view
                .iter()
                .map(|&i| l.entries[i].name.clone())
                .collect()
        }

        fn goto(&mut self, name: &str) {
            let Some(i) = self.names().iter().position(|n| n == name) else {
                panic!("{name} not in {:?}", self.names())
            };
            self.app.set_cursor(i);
        }
    }

    fn crossterm_char(c: char) -> ratatui::crossterm::event::KeyCode {
        ratatui::crossterm::event::KeyCode::Char(c)
    }

    fn status(app: &App) -> String {
        app.status
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn tab_visual_and_operands() {
        let (mut app, _rt) = fixture();
        // HIT, notes.txt, spec2.xlsx, spec10.xlsx (desktop.ini hidden)
        press(&mut app, "<Tab>");
        assert!(app.marked.contains("HIT"));
        assert_eq!(app.cursor, 1);
        press(&mut app, "2<Tab>");
        assert_eq!(app.marked.len(), 3);
        let screen = render(&mut app, 120, 20);
        assert!(screen.contains("3 selected"), "{screen}");
        assert!(screen.contains("▌"), "selection marker: {screen}");
        press(&mut app, "<Esc>");
        assert!(app.marked.is_empty());

        // v from row 1, two down: rows 1..=3; y yanks them and leaves visual.
        press(&mut app, "ggjvjj");
        assert_eq!(app.visual, Some(1));
        let screen = render(&mut app, 120, 20);
        assert!(
            screen.contains("VISUAL") && screen.contains("3 selected"),
            "{screen}"
        );
        press(&mut app, "y");
        assert_eq!(app.visual, None);
        let Some(reg) = &app.register else { panic!() };
        let names: Vec<String> = reg.items.iter().map(|i| i.path.name()).collect();
        assert_eq!(names, ["notes.txt", "spec2.xlsx", "spec10.xlsx"]);
        assert!(status(&app).contains("yanked 3 items"));

        // 2yy: two from the cursor; x cuts.
        press(&mut app, "gg2yy");
        let Some(reg) = &app.register else { panic!() };
        assert_eq!(reg.items.len(), 2);
        assert!(reg.items[0].is_dir, "HIT is a folder");
        press(&mut app, "x");
        assert_eq!(
            app.register.as_ref().map(|r| r.op),
            Some(crate::jobs::Op::Move)
        );
        // Pasting where they are is refused.
        press(&mut app, "p");
        assert!(
            status(&app).contains("already in this folder"),
            "{}",
            status(&app)
        );
        // `.` repeats the last copy-paste; nothing to repeat yet.
        press(&mut app, ".");
        assert!(
            status(&app).contains("nothing to repeat"),
            "{}",
            status(&app)
        );
        // Nothing to select in Locations.
        press(&mut app, "gl<Tab>");
        assert!(app.marked.is_empty());
    }

    #[test]
    fn yank_paste_runs_a_job_and_the_panel_shows_it() {
        let dir = temp_tree();
        let mut l = Live::new(&dir.join("src"));
        // a.txt, b.txt: a native copy.
        l.goto("a.txt");
        press(&mut l.app, "2yy");
        l.cd(&dir.join("dst"));
        press(&mut l.app, "p");
        l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
        let job = &l.app.jobs[0];
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        assert_eq!(job.backend, crate::jobs::Backend::Native);
        assert_eq!(
            std::fs::read_to_string(dir.join("dst").join("b.txt"))
                .ok()
                .as_deref(),
            Some("bb")
        );
        assert!(
            status(&l.app).starts_with("done: 2 files → dst"),
            "{}",
            status(&l.app)
        );
        // The destination listing refreshes by itself.
        l.until(|a| a.view.len() == 2);

        // Again: both are there now, so p skips them and says so.
        press(&mut l.app, "p");
        l.until(|a| a.jobs.len() == 2 && a.jobs[1].phase.is_over());
        assert!(status(&l.app).contains("2 skipped"), "{}", status(&l.app));

        // `.` pastes the same files again elsewhere.
        l.cd(&dir.join("src").join("proj"));
        press(&mut l.app, ".");
        l.until(|a| a.jobs.len() == 3 && a.jobs[2].phase.is_over());
        assert!(dir.join("src").join("proj").join("b.txt").exists());
        l.app.jobs.pop();
        let _ = std::fs::remove_file(dir.join("src").join("proj").join("a.txt"));
        let _ = std::fs::remove_file(dir.join("src").join("proj").join("b.txt"));

        // A folder goes through robocopy.
        l.cd(&dir.join("src"));
        l.goto("proj");
        press(&mut l.app, "yy");
        l.cd(&dir.join("dst"));
        press(&mut l.app, "p");
        l.until(|a| a.jobs.len() == 3 && a.jobs[2].phase.is_over());
        let job = &l.app.jobs[2];
        assert_eq!(
            (job.phase, job.backend),
            (Phase::Done, crate::jobs::Backend::Robocopy)
        );
        assert!(
            dir.join("dst")
                .join("proj")
                .join("sub")
                .join("c.txt")
                .exists()
        );

        // The panel: newest first, with its report.
        press(&mut l.app, "J");
        assert_eq!(l.app.mode, Mode::Jobs);
        let screen = render(&mut l.app, 140, 40);
        assert!(
            screen.contains("proj → dst") && screen.contains("DONE"),
            "{screen}"
        );
        assert!(
            screen.contains("FINISHED") && screen.contains("robocopy exit 1"),
            "{screen}"
        );
        // Enter dismisses the selected (newest) one.
        press(&mut l.app, "<CR>");
        assert_eq!(l.app.jobs.len(), 2);
        press(&mut l.app, "q");
        assert_eq!(l.app.mode, Mode::Normal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cut_and_paste_on_one_volume_renames() {
        let dir = temp_tree();
        let mut l = Live::new(&dir.join("src"));
        l.goto("proj");
        press(&mut l.app, "x");
        l.cd(&dir.join("dst"));
        press(&mut l.app, "p");
        l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
        assert_eq!(l.app.jobs[0].backend, crate::jobs::Backend::Rename);
        assert!(
            dir.join("dst")
                .join("proj")
                .join("sub")
                .join("c.txt")
                .exists()
        );
        assert!(!dir.join("src").join("proj").exists());
        assert!(l.app.register.is_none(), "a cut is pasted once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_and_aveva_ask_first() {
        let dir = temp_tree();
        let proj = dir.join("src").join("proj");
        let _ = std::fs::write(proj.join("evars.bat"), "set x=1");
        let _ = std::fs::create_dir_all(proj.join("hit000"));
        let mut l = Live::new(&dir.join("src"));
        l.goto("proj");
        press(&mut l.app, "yy");
        l.cd(&dir.join("dst"));
        press(&mut l.app, "P");
        l.until(|a| a.mode == Mode::Prompt);
        let note = l.app.confirm_note();
        assert!(note.contains("proj looks like an AVEVA project"), "{note}");
        assert!(note.contains("REPLACED"), "{note}");
        // Anything but y cancels.
        press(&mut l.app, "n");
        assert!(l.app.jobs.is_empty());
        press(&mut l.app, "P");
        l.until(|a| a.mode == Mode::Prompt);
        press(&mut l.app, "y");
        l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
        assert!(dir.join("dst").join("proj").join("evars.bat").exists());

        // A file-only paste with p asks nothing.
        l.cd(&dir.join("src"));
        l.goto("a.txt");
        press(&mut l.app, "yy");
        l.cd(&dir.join("dst"));
        press(&mut l.app, "p");
        assert_eq!(l.app.mode, Mode::Normal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jobs_panel_layout_wide_and_narrow() {
        let (mut app, _rt) = fixture();
        let plan = crate::jobs::Plan {
            backend: crate::jobs::Backend::Robocopy,
            steps: vec![crate::jobs::robocopy::Step {
                src: r"\\srv01\p\Videos".into(),
                dst: r"D:\local\test".into(),
                files: vec!["site-survey.avi".into()],
            }],
            pairs: vec![],
            title: "site-survey.avi → test".into(),
        };
        let mut job = crate::jobs::Job::new(
            9,
            &plan,
            crate::jobs::Op::Copy,
            r"D:\local\test".into(),
            crate::jobs::robocopy::Overwrite::Never,
            r"C:\logs\9.log".into(),
        );
        let t0 = Instant::now();
        const MB: u64 = 1_000_000;
        job.apply(
            JobEvent::Scanned {
                files: 1,
                bytes: 700 * MB,
            },
            t0,
        );
        job.apply(
            JobEvent::Output(vec![RcEvent::File {
                class: crate::jobs::robocopy::FileClass::New,
                size: 700 * MB,
                path: r"\\srv01\p\Videos\site-survey.avi".into(),
            }]),
            t0,
        );
        // 30 s at about 10 MB/s.
        for s in 1..=30u64 {
            let pct = (s as f32 * 10.0 / 700.0 * 100.0).min(99.0);
            job.apply(
                JobEvent::Output(vec![RcEvent::Progress(pct)]),
                t0 + Duration::from_secs(s),
            );
        }
        app.jobs.push(job);
        press(&mut app, "J");
        let wide = render(&mut app, 220, 50);
        let narrow = render(&mut app, 100, 50);
        if std::env::var_os("SENTIER_SHOW_JOBS").is_some() {
            eprintln!("{wide}\n{narrow}");
        }
        for want in [
            "RUNNING 1",
            "COPYING",
            "✓ Scanning",
            "● Copying",
            "THROUGHPUT",
            "STATUS",
            "PATHS",
            "ACTIVITY",
            r"\\srv01\p\Videos\site-survey.avi",
            "MB/s",
            "█",
        ] {
            assert!(wide.contains(want), "{want}\n{wide}");
        }
        assert!(
            narrow.contains("RUNNING 1") && narrow.contains("THROUGHPUT"),
            "{narrow}"
        );

        // Waiting on a locked file: STALLED, the banner says why, s / R reach the job.
        let ctl = crate::jobs::Control::default();
        let id = app.jobs[0].id;
        app.job_cancel.insert(id, ctl.clone());
        app.jobs[0].apply(
            JobEvent::Waiting {
                path: r"\\srv01\p\Videos\DEMO\dbs\demo1043.dbx".into(),
                code: 32,
                message: "The process cannot access the file because it is being used by another process.".into(),
                attempt: 3,
                max: 5,
                next: Instant::now() + Duration::from_secs(18),
            },
            Instant::now(),
        );
        let stall = render(&mut app, 220, 50);
        if std::env::var_os("SENTIER_SHOW_JOBS").is_some() {
            eprintln!("{stall}");
        }
        for want in [
            "STALLED",
            "◐ Copying",
            "⚠ No progress for",
            "waiting to retry a locked file",
            "error 32",
            "Try 3 of 5",
            "Everything else is copied",
            "s skip this file",
            "R retry now",
            "ETA waiting on a locked file",
            "s skip locked file",
            "⚠ 1 job stalled",
        ] {
            assert!(stall.contains(want), "{want}\n{stall}");
        }
        press(&mut app, "s");
        assert!(ctl.take_skip(), "s reached the runner");
        press(&mut app, "R");
        assert!(ctl.take_retry_now(), "R reached the runner");

        // p pauses (the runner confirms with Paused), p again resumes.
        press(&mut app, "p");
        assert!(ctl.paused());
        app.jobs[0].apply(JobEvent::Paused(true), Instant::now());
        let paused = render(&mut app, 220, 50);
        for want in ["PAUSED", "‖ Paused · p resumes", "ETA — paused", "p resume"] {
            assert!(paused.contains(want), "{want}\n{paused}");
        }
        press(&mut app, "p");
        assert!(!ctl.paused());

        // Finished with a file left out: `!` and SKIPPED.
        app.jobs[0].apply(
            JobEvent::Skipped(r"\\srv01\p\Videos\DEMO\dbs\demo1043.dbx".into()),
            Instant::now(),
        );
        app.jobs[0].apply(JobEvent::StepDone(1), Instant::now());
        app.jobs[0].apply(JobEvent::Finished { cancelled: false }, Instant::now());
        let done = render(&mut app, 220, 50);
        if std::env::var_os("SENTIER_SHOW_JOBS").is_some() {
            eprintln!("{done}");
        }
        assert!(done.contains("! site-survey.avi"), "{done}");
        assert!(
            done.contains("1 skipped") && done.contains("SKIPPED 1"),
            "{done}"
        );
    }

    #[test]
    fn finishing_failed_report_and_retry() {
        let (mut app, _rt) = fixture();
        let Ok(Loc::Fs(dest)) = Loc::parse(r"D:\local") else {
            panic!()
        };
        let plan = crate::jobs::Plan {
            backend: crate::jobs::Backend::Robocopy,
            steps: vec![crate::jobs::robocopy::Step {
                src: r"\\srv01\p\HIT".into(),
                dst: r"D:\local\HIT".into(),
                files: vec![],
            }],
            pairs: vec![],
            title: "HIT → local".into(),
        };
        let mut job = crate::jobs::Job::new(
            7,
            &plan,
            crate::jobs::Op::Copy,
            dest.display(),
            crate::jobs::robocopy::Overwrite::Never,
            r"C:\logs\7.log".into(),
        );
        let now = Instant::now();
        job.apply(
            JobEvent::Scanned {
                files: 2,
                bytes: 300,
            },
            now,
        );
        let file = |p: &str, size| RcEvent::File {
            class: FileClass::New,
            size,
            path: p.into(),
        };
        job.apply(
            JobEvent::Output(vec![
                file(r"\\srv01\p\HIT\a.bin", 100),
                RcEvent::Progress(100.0),
                file(r"\\srv01\p\HIT\hitmac\b.bin", 200),
                RcEvent::Progress(100.0),
            ]),
            now,
        );
        assert_eq!(job.phase, Phase::Finishing);
        app.jobs.push(job);
        press(&mut app, "J");
        let screen = render(&mut app, 140, 40);
        assert!(screen.contains("FINISHING"), "{screen}");
        assert!(screen.contains("every byte is written"), "{screen}");
        assert!(screen.contains("● Finishing"), "{screen}");
        assert!(render(&mut app, 140, 40).contains("⇅ 1 job · 100% finishing"));

        // It fails on a locked file after all: the report lists it, r retries only it.
        let j = &mut app.jobs[0];
        j.apply(
            JobEvent::Output(vec![
                file(r"\\srv01\p\HIT\hitmac\c.pmlmac", 6),
                RcEvent::Error(crate::jobs::robocopy::RcError {
                    code: 32,
                    action: "Copying File".into(),
                    path: Some(r"\\srv01\p\HIT\hitmac\c.pmlmac".into()),
                    message: "The process cannot access the file because it is being used by another process.".into(),
                }),
            ]),
            now,
        );
        j.apply(JobEvent::StepDone(9), now);
        j.apply(JobEvent::Finished { cancelled: false }, now);
        let screen = render(&mut app, 160, 40);
        assert!(
            screen.contains("FAILED robocopy exit 9  =  1 (files copied) + 8 (some copies failed)"),
            "{screen}"
        );
        assert!(
            screen.contains("hitmac\\c.pmlmac") && screen.contains("error 32 · The process"),
            "{screen}"
        );
        assert!(screen.contains("r retries them"), "{screen}");
        press(&mut app, "r");
        assert_eq!(app.jobs.len(), 2);
        let retry = &app.jobs[1];
        assert_eq!(retry.title, "retry: HIT → local");
        assert_eq!(retry.steps.len(), 1);
        assert_eq!(retry.steps[0].src, r"\\srv01\p\HIT\hitmac");
        assert_eq!(retry.steps[0].dst, r"D:\local\HIT\hitmac");
        assert_eq!(retry.steps[0].files, ["c.pmlmac"]);
        // c cancels the running retry.
        press(&mut app, "gc");
        assert!(
            status(&app).contains("cancelling retry"),
            "{}",
            status(&app)
        );
    }

    // ------------------------------------------------------------ file operations

    fn prompt_text(app: &App) -> (String, usize, String) {
        app.prompt
            .as_ref()
            .map(|p| (p.editor.text(), p.editor.cursor(), p.note.clone()))
            .unwrap_or_default()
    }

    fn selected_name(app: &App) -> String {
        app.selected().map(|e| e.name.clone()).unwrap_or_default()
    }

    #[test]
    fn rename_new_and_delete() {
        let dir = temp_tree();
        let src = dir.join("src");
        let mut l = Live::new(&src);
        // r: the cursor starts before the extension.
        l.goto("a.txt");
        press(&mut l.app, "r");
        assert_eq!(prompt_text(&l.app).0, "a.txt");
        assert_eq!(prompt_text(&l.app).1, 1);
        press(&mut l.app, "x<CR>");
        l.until(|a| a.ops_running == 0 && selected_name(a) == "ax.txt");
        assert!(src.join("ax.txt").exists() && !src.join("a.txt").exists());
        // cw too; a bad name keeps the prompt open and says why.
        press(&mut l.app, "cw<C-u>con<CR>");
        assert_eq!(l.app.mode, Mode::Prompt);
        assert!(
            prompt_text(&l.app).2.contains("reserved"),
            "{:?}",
            prompt_text(&l.app)
        );
        press(&mut l.app, "<Esc><Esc>");
        assert_eq!(l.app.mode, Mode::Normal);

        // a: a file, with a new parent folder; the cursor lands on what appeared.
        press(&mut l.app, "a");
        for c in r"notes\todo.txt".chars() {
            l.app.handle(AppEvent::Term(Event::Key(KeyEvent::new(
                crossterm_char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ))));
        }
        press(&mut l.app, "<CR>");
        l.until(|a| a.ops_running == 0 && selected_name(a) == "notes");
        assert!(src.join("notes").join("todo.txt").is_file());
        // a with a trailing \ and A: folders.
        press(&mut l.app, "a");
        for c in r"made\".chars() {
            l.app.handle(AppEvent::Term(Event::Key(KeyEvent::new(
                crossterm_char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ))));
        }
        press(&mut l.app, "<CR>");
        l.until(|a| a.ops_running == 0 && selected_name(a) == "made");
        press(&mut l.app, "Afolder<CR>");
        l.until(|a| a.ops_running == 0 && selected_name(a) == "folder");
        assert!(src.join("made").is_dir() && src.join("folder").is_dir());
        // An existing name fails with a message.
        press(&mut l.app, "Afolder<CR>");
        l.until(|a| a.ops_running == 0);
        assert!(
            status(&l.app).contains("already exists"),
            "{}",
            status(&l.app)
        );

        // dd asks (Recycle Bin, local); n keeps it.
        l.goto("b.txt");
        press(&mut l.app, "dd");
        assert!(
            l.app
                .confirm_note()
                .contains("move b.txt to the Recycle Bin?"),
            "{}",
            l.app.confirm_note()
        );
        press(&mut l.app, "n");
        assert!(src.join("b.txt").exists());
        // Visual d: the range.
        l.goto("folder");
        press(&mut l.app, "vjd");
        assert!(
            l.app
                .confirm_note()
                .contains("move 2 items to the Recycle Bin?"),
            "{}",
            l.app.confirm_note()
        );
        press(&mut l.app, "n");
        // dd shows a popup naming what goes; Esc keeps it.
        l.goto("b.txt");
        press(&mut l.app, "dd");
        let screen = render(&mut l.app, 120, 30);
        assert!(
            screen.contains("Move to the Recycle Bin") && screen.contains("• b.txt"),
            "{screen}"
        );
        assert!(screen.contains("Yes") && screen.contains("No"), "{screen}");
        press(&mut l.app, "x");
        assert_eq!(l.app.mode, Mode::Prompt, "a stray key isn't an answer");
        press(&mut l.app, "<Esc>");
        assert!(src.join("b.txt").exists());
        // D: a red popup that starts on No; Enter alone keeps it, y deletes.
        l.goto("made");
        press(&mut l.app, "D");
        let d = l.app.dialog_view();
        assert!(d.as_ref().is_some_and(|d| d.danger && !d.yes), "{d:?}");
        assert!(d.is_some_and(|d| d.title == "Delete permanently"
            && d.message.contains("PERMANENTLY")
            && d.items == ["made"]));
        press(&mut l.app, "<CR>");
        assert!(src.join("made").exists(), "Enter on No keeps it");
        // l (or Tab) moves to Yes; Enter presses it. Here: straight `y`.
        press(&mut l.app, "Dl");
        assert!(l.app.dialog_view().is_some_and(|d| d.yes));
        press(&mut l.app, "<Esc>Dy");
        l.until(|a| a.ops_running == 0 && !a.view.is_empty() && !src.join("made").exists());
        assert!(status(&l.app).contains("deleted"), "{}", status(&l.app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bulk_rename_through_the_editor() {
        let dir = temp_tree();
        let src = dir.join("src");
        let mut l = Live::new(&src);
        // Two selected: a.txt, b.txt.
        l.goto("a.txt");
        press(&mut l.app, "2<Tab>:rename-edit<CR>");
        let Some(req) = l.app.inline.take() else {
            panic!("no editor request")
        };
        let (_, text) = req.file.clone().unwrap_or_default();
        assert_eq!(text, "a.txt\r\nb.txt\r\n");
        assert_eq!(req.args.last().map(|a| a.ends_with(".txt")), Some(true));
        // The user swaps them in the editor.
        l.app.inline_done(
            req.then,
            fileops::InlineResult {
                status: Ok(0),
                text: Some("b.txt\r\na.txt\r\n".into()),
            },
        );
        assert!(
            l.app
                .confirm_note()
                .contains("rename a.txt → b.txt, b.txt → a.txt?"),
            "{}",
            l.app.confirm_note()
        );
        press(&mut l.app, "y");
        l.until(|a| a.ops_running == 0);
        assert_eq!(
            std::fs::read_to_string(src.join("a.txt")).ok().as_deref(),
            Some("bb")
        );
        assert_eq!(
            std::fs::read_to_string(src.join("b.txt")).ok().as_deref(),
            Some("a")
        );

        // A wrong line count renames nothing.
        press(&mut l.app, ":rename-edit<CR>");
        let Some(req) = l.app.inline.take() else {
            panic!()
        };
        l.app.inline_done(
            req.then,
            fileops::InlineResult {
                status: Ok(0),
                text: Some("x\n".into()),
            },
        );
        assert!(
            status(&l.app).contains("one line per name"),
            "{}",
            status(&l.app)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_inline_suspends_and_reads_back() {
        struct Count(std::sync::atomic::AtomicU32);
        impl fileops::Suspend for Count {
            fn suspend(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn resume(&self) {
                self.0.fetch_add(10, Ordering::SeqCst);
            }
        }
        let dir = temp_tree();
        let file = dir.join("list.txt");
        let req = fileops::Inline {
            program: "cmd".into(),
            args: vec![
                "/D".into(),
                "/C".into(),
                format!("echo z.txt>{}", file.display()),
            ],
            file: Some((file.clone(), "a.txt\r\n".into())),
            then: fileops::InlineThen::BulkRename {
                dir: match Loc::parse(&dir.display().to_string()) {
                    Ok(Loc::Fs(p)) => p,
                    _ => panic!(),
                },
                names: vec!["a.txt".into()],
            },
        };
        let c = Count(std::sync::atomic::AtomicU32::new(0));
        let r = fileops::run_inline(&req, &c);
        assert_eq!(
            c.0.load(Ordering::SeqCst),
            11,
            "suspended once, resumed once"
        );
        assert_eq!(r.status, Ok(0));
        assert_eq!(r.text.as_deref().map(str::trim), Some("z.txt"));
        assert!(!file.exists(), "the list is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn network_and_admin_share_questions() {
        let (mut app, _rt) = fixture();
        let show = |app: &mut App, path: &str, names: &[&str]| {
            let Ok(loc) = Loc::parse(path) else { panic!() };
            let entries = names
                .iter()
                .map(|n| super::tests::entry(n, EntryKind::File, 1, false))
                .collect();
            app.dirs.insert(
                loc.clone(),
                DirState {
                    listing: Some(super::tests::listing(entries)),
                    ..DirState::default()
                },
            );
            app.set_cwd(loc, None);
            app.pending.clear();
        };
        show(&mut app, r"\\srv01\p\HIT", &["x.pdf"]);
        press(&mut app, "dd");
        let note = app.confirm_note();
        assert!(
            note.contains("delete x.pdf? Permanent: no Recycle Bin over the network."),
            "{note}"
        );
        press(&mut app, "n");

        // An admin share names the machine (and its user once known) for every change.
        show(&mut app, r"\\PC-042\C$\Users\jdoe", &["x.txt"]);
        app.handle(AppEvent::MachineUsers {
            host: "PC-042".into(),
            result: Ok(crate::fs::worker::MachineUsers {
                logged_on: vec!["jdoe".into()],
                profiles: vec![],
                likely: None,
            }),
        });
        press(&mut app, "dd");
        let note = app.confirm_note();
        assert!(
            note.starts_with("On PC-042 (jdoe's machine): delete x.txt? Permanent"),
            "{note}"
        );
        press(&mut app, "n");
        press(&mut app, "r<C-e><C-u>y.txt<CR>");
        let note = app.confirm_note();
        assert_eq!(app.mode, Mode::Prompt);
        assert!(
            note.contains("On PC-042 (jdoe's machine): rename x.txt → y.txt?"),
            "{note}"
        );
        press(&mut app, "n");
        // A paste there asks too.
        app.register = Some(super::jobs::Register {
            op: crate::jobs::Op::Copy,
            items: vec![crate::jobs::Item {
                path: match Loc::parse(r"C:\work\notes.txt") {
                    Ok(Loc::Fs(p)) => p,
                    _ => panic!(),
                },
                is_dir: false,
                size: 3,
            }],
        });
        press(&mut app, "p");
        let note = app.confirm_note();
        assert!(
            note.starts_with("On PC-042 (jdoe's machine): copy notes.txt → jdoe here."),
            "{note}"
        );
        press(&mut app, "n");
        press(&mut app, "Anew<CR>");
        assert!(app.confirm_note().contains("create folder new?"));
        press(&mut app, "n");
        assert_eq!(status(&app), "unchanged");
        // Nothing to delete in Locations' sense with D.
        press(&mut app, "gl");
        press(&mut app, "D");
        assert!(status(&app).contains("in Locations dd removes"));
    }

    fn preview_done(a: &App) -> bool {
        a.sync_preview
            .as_ref()
            .is_some_and(|s| !matches!(s.status, PreviewStatus::Running))
    }

    /// In the preview, `x` leaves a file out, `X` its folder, and on a local-only file `x`
    /// keeps mirror from deleting it: saved in the pin (full paths), the dry run redone,
    /// honoured by the real run (robocopy); `E` brings everything back.
    #[test]
    fn sync_preview_leaves_files_out() {
        use crate::jobs::preset::SyncPreset;
        let dir = temp_tree();
        let local = dir.join("dst");
        let _ = std::fs::write(local.join("stale.txt"), "old");
        let mut l = Live::new(&dir);
        l.until(|a| a.pins_loaded);
        let text = format!(
            "[[pin]]\nkind = 'sync'\nname = 'proj sync'\nsource = '{}'\ndest = '{}'\nmode = 'mirror'\n",
            dir.join("src").display(),
            local.display()
        );
        let Ok((store, _)) = crate::pins::parse(&text) else {
            panic!("{text}")
        };
        l.app.pins = store;
        let rows = |a: &App| -> Vec<String> {
            a.sync_preview
                .as_ref()
                .map(|s| s.listed().1.iter().map(|r| r.rel.clone()).collect())
                .unwrap_or_default()
        };
        let goto_row = |a: &mut App, rel: &str| {
            let i = rows(a).iter().position(|r| r == rel);
            if let (Some(sp), Some(i)) = (a.sync_preview.as_mut(), i) {
                sp.scroll = i;
            }
            i.is_some()
        };
        press(&mut l.app, ":sync proj<CR>");
        l.until(preview_done);
        // Mirror's short list: the deletion. x keeps stale.txt.
        assert!(goto_row(&mut l.app, "stale.txt"), "{:?}", rows(&l.app));
        let screen = render(&mut l.app, 140, 40);
        assert!(
            screen.contains("x / X") && screen.contains("Leave out"),
            "{screen}"
        );
        press(&mut l.app, "x");
        l.until(preview_done);
        assert!(
            l.app.pins.pins[0]
                .extra
                .get("exclude_files")
                .and_then(|v| v.as_array())
                .is_some_and(|a| a
                    .iter()
                    .any(|v| v.as_str().is_some_and(|s| s.ends_with(r"dst\stale.txt")))),
            "{:?}",
            l.app.pins.pins[0].extra
        );
        // Every change (the list and row stay across the redone dry runs): x on a.txt,
        // X on proj\sub\c.txt (its folder).
        press(&mut l.app, "<Tab>");
        assert!(goto_row(&mut l.app, "a.txt"), "{:?}", rows(&l.app));
        press(&mut l.app, "x");
        l.until(preview_done);
        assert!(
            l.app.sync_preview.as_ref().is_some_and(|s| s.show_all),
            "still every change"
        );
        assert!(
            goto_row(&mut l.app, r"proj\sub\c.txt"),
            "{:?}",
            rows(&l.app)
        );
        press(&mut l.app, "X");
        l.until(preview_done);
        assert_eq!(rows(&l.app), ["b.txt"], "only b.txt still syncs");
        let screen = render(&mut l.app, 140, 40);
        assert!(screen.contains("Bring back 3 left out"), "{screen}");
        let Ok(p) = SyncPreset::from_pin(&l.app.pins.pins[0]) else {
            panic!()
        };
        assert_eq!(p.exclude_files.len(), 2);
        assert_eq!(
            p.exclude_dirs,
            [dir.join("src")
                .join("proj")
                .join("sub")
                .display()
                .to_string()]
        );
        // The real run honours it.
        press(&mut l.app, "<CR>");
        l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
        assert!(local.join("b.txt").exists());
        assert!(!local.join("a.txt").exists(), "left out");
        assert!(
            !local.join("proj").join("sub").join("c.txt").exists(),
            "folder left out"
        );
        assert!(
            local.join("stale.txt").exists(),
            "kept from the mirror's delete"
        );
        // E: everything back.
        press(&mut l.app, ":sync proj<CR>");
        l.until(preview_done);
        press(&mut l.app, "E");
        l.until(preview_done);
        let Ok(p) = SyncPreset::from_pin(&l.app.pins.pins[0]) else {
            panic!()
        };
        assert!(p.exclude_files.is_empty() && p.exclude_dirs.is_empty());
        press(&mut l.app, "<Tab>");
        assert!(
            rows(&l.app).contains(&"a.txt".to_string()),
            "{:?}",
            rows(&l.app)
        );
        press(&mut l.app, "<Esc>");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sync preset end to end with real robocopy: dry run, nothing touched, `u` to
    /// update (keeps the local-only file), then mirror (deletes it), last runs kept.
    #[test]
    fn sync_preset_previews_runs_and_remembers() {
        use crate::jobs::preset::{Change, SyncMode};
        let dir = temp_tree();
        let local = dir.join("dst");
        let _ = std::fs::write(local.join("stale.txt"), "old");
        let mut l = Live::new(&dir);
        l.until(|a| a.pins_loaded);
        let text = format!(
            "[[pin]]\nkind = 'sync'\nname = 'proj sync'\nsource = '{}'\ndest = '{}'\n\
             mode = 'mirror'\nkey = 's'\n",
            dir.join("src").display(),
            local.display()
        );
        let Ok((store, notes)) = crate::pins::parse(&text) else {
            panic!("{text}")
        };
        assert!(notes.is_empty(), "{notes:?}");
        l.app.pins = store;

        press(&mut l.app, ":sync proj<CR>");
        assert_eq!(l.app.mode, Mode::SyncPreview);
        l.until(preview_done);
        let Some(pv) = l
            .app
            .sync_preview
            .as_ref()
            .and_then(|s| s.preview().cloned())
        else {
            panic!("{:?}", l.app.sync_preview.as_ref().map(|s| &s.status))
        };
        assert_eq!(pv.to_copy(), (3, 4));
        assert_eq!(pv.count(Change::Extra).0, 1);
        assert!(
            pv.rows.iter().any(|r| r.rel == r"proj\sub\c.txt"),
            "{:?}",
            pv.rows
        );
        assert!(
            local.join("stale.txt").exists(),
            "a dry run touches nothing"
        );
        assert!(!local.join("a.txt").exists());
        let screen = render(&mut l.app, 120, 40);
        assert!(screen.contains("Sync preview · proj sync"), "{screen}");
        assert!(screen.contains("local files will be DELETED"), "{screen}");
        assert!(screen.contains("- stale.txt"), "{screen}");

        // `u`: this run updates; the local-only file is kept.
        press(&mut l.app, "u");
        let screen = render(&mut l.app, 120, 40);
        assert!(screen.contains("local-only files, kept"), "{screen}");
        press(&mut l.app, "<CR>");
        assert!(l.app.sync_preview.is_none());
        l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
        let job = &l.app.jobs[0];
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        assert_eq!(job.total_files, Some(3), "totals came from the preview");
        assert_eq!(
            job.preset.as_ref().map(|p| p.label.as_str()),
            Some("sync · update")
        );
        assert!(local.join("proj").join("sub").join("c.txt").exists());
        assert!(local.join("stale.txt").exists(), "update never deletes");
        let key = crate::jobs::preset::SyncPreset::from_pin(&l.app.pins.pins[0]).map(|p| p.key());
        let run = key.ok().and_then(|k| l.app.preset_runs.get(&k).cloned());
        assert_eq!(run.map(|r| (r.phase, r.files)), Some((Phase::Done, 3)));
        assert!(
            l.app
                .last_run_line(&l.app.pins.pins[0])
                .is_some_and(|s| s.starts_with("today ") && s.contains('✓'))
        );

        // Mirror now: nothing to copy, the stale file goes.
        press(&mut l.app, "'s");
        assert_eq!(l.app.mode, Mode::SyncPreview);
        l.until(preview_done);
        assert_eq!(
            l.app.sync_preview.as_ref().map(|s| s.mode),
            Some(SyncMode::Mirror)
        );
        press(&mut l.app, "<CR>");
        l.until(|a| a.jobs.len() == 2 && a.jobs.iter().all(|j| j.phase.is_over()));
        assert!(
            !local.join("stale.txt").exists(),
            "mirror deletes local extras"
        );
        assert!(local.join("a.txt").exists());

        // Up to date: Enter just closes. `:sync!` on a mirror preset still previews.
        press(&mut l.app, ":sync! proj sync<CR>");
        assert_eq!(l.app.mode, Mode::SyncPreview);
        l.until(preview_done);
        press(&mut l.app, "<CR>");
        assert!(status(&l.app).contains("up to date"), "{}", status(&l.app));
        assert_eq!(l.app.jobs.len(), 2);
        press(&mut l.app, ":sync proj<CR>");
        press(&mut l.app, "<Esc>");
        assert_eq!(l.app.mode, Mode::Normal);
        assert!(status(&l.app).contains("nothing was touched"));
        press(&mut l.app, ":sync nope<CR>");
        assert!(status(&l.app).contains("no sync preset matches"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backup preset from `:backup` to a verified zip, its report and last run; a
    /// source that is a sync preset's local copy is zipped without staging.
    #[test]
    fn backup_preset_asks_then_zips() {
        let dir = temp_tree();
        let zips = dir.join("zips");
        let mut l = Live::new(&dir);
        l.until(|a| a.pins_loaded);
        let text = format!(
            "[[pin]]\nkind = 'backup'\nname = 'src backup'\nsource = '{src}'\n\
             dest_dir = '{zips}'\nkeep = 3\n\n\
             [[pin]]\nkind = 'sync'\nname = 'other'\nsource = 'C:\\nowhere'\ndest = '{dst}'\n",
            src = dir.join("src").display(),
            zips = zips.display(),
            dst = dir.join("dst").display(),
        );
        let Ok((store, notes)) = crate::pins::parse(&text) else {
            panic!("{text}")
        };
        assert!(notes.is_empty(), "{notes:?}");
        l.app.pins = store;

        press(&mut l.app, ":backup src<CR>");
        let note = l.app.confirm_note();
        assert!(
            note.contains("Back up") && note.contains("staged locally first"),
            "{note}"
        );
        assert!(note.contains("keeps the newest 3"), "{note}");
        press(&mut l.app, "y");
        l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
        let job = &l.app.jobs[0];
        assert_eq!(job.phase, Phase::Done, "{:?}", job.error);
        let made: Vec<String> = std::fs::read_dir(&zips)
            .map(|r| {
                r.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(made[0].starts_with("src_") && made[0].ends_with(".zip"));
        let r = job.report(Instant::now());
        assert_eq!(r.backup.as_ref().map(|b| b.entries), Some(5));
        assert!(
            l.app
                .last_run_line(&l.app.pins.pins[0])
                .is_some_and(|s| s.contains('✓')),
        );
        press(&mut l.app, "J");
        let screen = render(&mut l.app, 160, 40);
        assert!(
            screen.contains("zip ") && screen.contains("5 entries"),
            "{screen}"
        );
        assert!(screen.contains("every CRC checked"), "{screen}");
        press(&mut l.app, "J");

        // Pointed at a sync preset's local copy: no staging.
        let Ok(Loc::Fs(dst)) = Loc::parse(&dir.join("dst").display().to_string()) else {
            panic!()
        };
        l.app.pins.pins[0]
            .extra
            .insert("source".into(), toml::Value::String(dst.display()));
        press(&mut l.app, ":backup src backup<CR>");
        assert!(l.app.confirm_note().contains("zipped directly"));
        press(&mut l.app, "n");
        assert_eq!(l.app.jobs.len(), 1, "n runs nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Types `text` into whatever has the keyboard, one character at a time.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle(AppEvent::Term(Event::Key(KeyEvent::new(
                crossterm_char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ))));
        }
    }

    fn line_now(app: &App) -> String {
        app.prompt
            .as_ref()
            .map(|p| p.editor.text())
            .unwrap_or_default()
    }

    fn prompt_note(app: &App) -> String {
        app.prompt
            .as_ref()
            .map(|p| p.note.clone())
            .unwrap_or_default()
    }

    /// `:sync new` / `:backup new` and `^e` build presets in the app. Folders are chosen
    /// by going there (pick mode, `p`), refused when they nest; the short questions are
    /// prompts; the name comes last, suggested from the folders. A selection becomes
    /// `include`; Esc drops it all.
    #[test]
    fn presets_are_made_and_edited_in_the_app() {
        use crate::jobs::preset::{SyncMode, SyncPreset};
        use crate::jobs::zip::{BackupPreset, Engine};
        let dir = temp_tree();
        let _ = std::fs::create_dir_all(dir.join("zips"));
        let mut l = Live::new(&dir);
        l.until(|a| a.pins_loaded);
        l.app.pins = crate::pins::PinStore::default();
        l.cd(&dir.join("src"));

        // A sync from here: straight into picking the destination.
        press(&mut l.app, ":sync new<CR>");
        assert!(l.app.pick.is_some());
        assert!(l.app.prompt.is_none(), "nothing to type");
        let screen = render(&mut l.app, 160, 20);
        assert!(
            screen.contains("PICK  sync src → ?") && screen.contains("then p"),
            "{screen}"
        );
        assert!(screen.contains("use this folder"), "the hint row: {screen}");
        // Here, or inside the source: refused, still picking.
        press(&mut l.app, "p");
        assert!(status(&l.app).contains("same folder"), "{}", status(&l.app));
        l.cd(&dir.join("src").join("proj"));
        press(&mut l.app, "p");
        assert!(
            status(&l.app).contains("inside source"),
            "{}",
            status(&l.app)
        );
        assert!(l.app.pick.is_some());
        // The destination: go there, p.
        l.cd(&dir.join("dst"));
        press(&mut l.app, "p");
        assert!(l.app.pick.is_none());
        assert_eq!(line_now(&l.app), "update");
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "mirror");
        assert!(prompt_note(&l.app).contains("DELETED"));
        press(&mut l.app, "<CR>");
        // The name comes last, from both folders.
        assert_eq!(line_now(&l.app), "src → dst");
        press(&mut l.app, "<CR>");
        type_text(&mut l.app, "aveva");
        press(&mut l.app, "<CR>");
        type_text(&mut l.app, "z");
        press(&mut l.app, "<CR>");
        assert_eq!(l.app.mode, Mode::Normal);
        assert_eq!(l.app.pins.pins.len(), 1);
        let pin = l.app.pins.pins[0].clone();
        let Ok(p) = SyncPreset::from_pin(&pin) else {
            panic!("{pin:?}")
        };
        assert_eq!(
            (p.mode, p.source.name(), p.dest.name()),
            (SyncMode::Mirror, "src".to_string(), "dst".to_string())
        );
        assert!(p.include.is_empty(), "nothing selected: the whole folder");
        assert_eq!(
            (pin.name.as_str(), pin.key_char(), pin.tags.clone()),
            ("src → dst", Some('z'), vec!["aveva".to_string()])
        );

        // ^e: the name, then each folder by going there (it starts where it was).
        l.app.edit_pin(0, false, None);
        press(&mut l.app, "<CR>");
        l.until(|a| a.pick.is_some() && a.cwd.name() == "src");
        press(&mut l.app, "p");
        l.until(|a| a.pick.is_some() && a.cwd.name() == "dst");
        press(&mut l.app, "p");
        assert_eq!(line_now(&l.app), "mirror");
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "u");
        press(&mut l.app, "<CR><CR><CR>");
        assert_eq!(l.app.mode, Mode::Normal);
        let p = SyncPreset::from_pin(&l.app.pins.pins[0]).map(|p| p.mode);
        assert_eq!(p, Ok(SyncMode::Update));
        assert_eq!(l.app.pins.pins[0].key_char(), Some('z'), "key kept");

        // A backup, with the engine offered because 7-Zip is "installed".
        l.app.seven_zip = Some(r"C:\Program Files\7-Zip\7z.exe".into());
        l.cd(&dir.join("src"));
        press(&mut l.app, ":backup new<CR>");
        let screen = render(&mut l.app, 160, 20);
        assert!(screen.contains("PICK  backup src → ?"), "{screen}");
        press(&mut l.app, "p");
        assert!(
            status(&l.app).contains("zips can't go into"),
            "{}",
            status(&l.app)
        );
        l.cd(&dir.join("zips"));
        press(&mut l.app, "p");
        assert_eq!(line_now(&l.app), "10");
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "x");
        press(&mut l.app, "<CR>");
        assert!(prompt_note(&l.app).contains("a number"));
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "3");
        press(&mut l.app, "<CR>");
        assert_eq!(line_now(&l.app), "builtin");
        assert!(prompt_note(&l.app).contains("7zip: faster"));
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "7zip");
        press(&mut l.app, "<CR>");
        assert_eq!(line_now(&l.app), "src backup");
        press(&mut l.app, "<CR><CR><CR>");
        assert_eq!(l.app.pins.pins.len(), 2);
        let Ok(b) = BackupPreset::from_pin(&l.app.pins.pins[1]) else {
            panic!("{:?}", l.app.pins.pins[1])
        };
        assert_eq!((b.keep, b.engine), (3, Engine::SevenZip));

        // A selection: only those are synced (a folder is its own step, files one more).
        l.cd(&dir.join("src"));
        l.goto("proj");
        press(&mut l.app, "<Tab>");
        l.goto("b.txt");
        press(&mut l.app, "<Tab>");
        press(&mut l.app, ":sync new<CR>");
        let screen = render(&mut l.app, 160, 20);
        assert!(
            screen.contains("PICK  sync src (2 selected) → ?"),
            "{screen}"
        );
        l.cd(&dir.join("dst"));
        press(&mut l.app, "p<CR><CR><CR><CR>");
        assert_eq!(l.app.pins.pins.len(), 3);
        let Ok(p) = SyncPreset::from_pin(&l.app.pins.pins[2]) else {
            panic!("{:?}", l.app.pins.pins[2])
        };
        assert_eq!(p.include, [r"proj\", "b.txt"]);
        let steps = p.steps();
        assert_eq!(steps.len(), 2);
        assert!(steps[0].src.ends_with(r"src\proj") && steps[0].dst.ends_with(r"dst\proj"));
        assert_eq!(steps[1].files, ["b.txt"]);

        // Esc while picking saves nothing; Locations isn't a folder to sync.
        l.cd(&dir.join("src"));
        press(&mut l.app, ":sync new<CR>");
        assert!(l.app.pick.is_some());
        press(&mut l.app, "<Esc>");
        assert!(l.app.pick.is_none());
        assert_eq!(l.app.pins.pins.len(), 3);
        assert!(status(&l.app).contains("not pinned"), "{}", status(&l.app));
        press(&mut l.app, "gl");
        press(&mut l.app, ":sync new<CR>");
        assert!(
            status(&l.app).contains("go to the folder"),
            "{}",
            status(&l.app)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `o` runs a file's first opener (inline here: the TUI would suspend), on the
    /// selection too; `O` lists the rule's openers, remembers the pick per extension,
    /// greys a missing program, and filters as you type.
    #[test]
    fn open_with_first_opener_and_the_menu() {
        let dir = temp_tree();
        let mut l = Live::new(&dir);
        l.cd(&dir.join("src"));
        let (cfg, problems) = crate::open::rules::parse(
            r#"
            [opener.echo]
            desc = "Echo"
            run = ["cmd", "/c", "echo", "{paths}"]
            mode = "inline"
            [opener.gone]
            desc = "Gone"
            run = ['C:\nope\gone.exe', "{path}"]
            [[open.rule]]
            match = "*.txt"
            use = ["echo", "gone", "default", "copy-path"]
            "#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        l.app.open_cfg = cfg;
        l.app.open_resolved.insert("gone".into(), None);
        // What Windows has for .txt, known already (no lookup racing the test).
        l.app.open_registered.insert(
            "txt".into(),
            crate::open::Registered {
                default_app: Some("Notepad".into()),
                apps: vec![("np".into(), "Notepad".into())],
            },
        );
        let a = dir.join("src").join("a.txt").display().to_string();
        let b = dir.join("src").join("b.txt").display().to_string();

        // o on a.txt: the inline opener, with its path.
        l.goto("a.txt");
        press(&mut l.app, "o");
        let Some(req) = l.app.inline.take() else {
            panic!("inline opener expected; status {}", status(&l.app))
        };
        assert_eq!(req.program, "cmd");
        assert_eq!(req.args, ["/c", "echo", a.as_str()]);
        // A selection: every selected file, one process (for = all).
        press(&mut l.app, "<Tab><Tab>");
        l.goto("a.txt");
        press(&mut l.app, "<CR>");
        let Some(req) = l.app.inline.take() else {
            panic!()
        };
        assert_eq!(req.args, ["/c", "echo", a.as_str(), b.as_str()]);
        press(&mut l.app, "<Esc>");

        // O: the menu.
        l.goto("a.txt");
        press(&mut l.app, "O");
        assert_eq!(l.app.mode, Mode::OpenWith);
        let ids = |app: &App| -> Vec<String> {
            app.open_menu
                .as_ref()
                .map(|m| m.items.iter().map(|i| i.id.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            ids(&l.app),
            [
                "echo",
                "gone",
                "default",
                "copy-path",
                "editor",
                "assoc:np",
                "openwith",
                "properties"
            ]
        );
        let screen = render(&mut l.app, 140, 40);
        assert!(screen.contains("Open a.txt with"), "{screen}");
        assert!(screen.contains("configured (*.txt)"), "{screen}");
        assert!(screen.contains("not found"), "{screen}");
        assert!(
            screen.contains("Windows suggests (registered for .txt)"),
            "{screen}"
        );
        assert!(screen.contains("Notepad"), "{screen}");
        // A later answer from Windows updates the open menu; the cursor stays on its item.
        press(&mut l.app, "<Down><Down>");
        l.app.handle(AppEvent::Registered {
            ext: "txt".into(),
            registered: crate::open::Registered {
                default_app: Some("Notepad".into()),
                apps: vec![
                    ("np".into(), "Notepad".into()),
                    ("code".into(), "VS Code".into()),
                ],
            },
        });
        assert_eq!(ids(&l.app)[6], "assoc:code");
        assert_eq!(l.app.open_menu.as_ref().map(|m| m.cursor), Some(2));
        press(&mut l.app, "<Up><Up>");
        // The missing one can't run: the menu stays.
        press(&mut l.app, "2");
        assert_eq!(l.app.mode, Mode::OpenWith);
        assert!(status(&l.app).contains("not found"));
        // Typing filters; Esc clears the filter first, then closes.
        type_text(&mut l.app, "ech");
        assert_eq!(l.app.open_menu.as_ref().map(|m| m.shown().len()), Some(1));
        press(&mut l.app, "<Esc>");
        assert_eq!(l.app.open_menu.as_ref().map(|m| m.shown().len()), Some(9));
        // 1 picks Echo, and it's remembered for .txt.
        press(&mut l.app, "1");
        assert_eq!(l.app.mode, Mode::Normal);
        assert!(l.app.inline.take().is_some());
        assert_eq!(l.app.open_last.get("txt").map(String::as_str), Some("echo"));
        press(&mut l.app, "O");
        assert!(
            l.app
                .open_menu
                .as_ref()
                .is_some_and(|m| m.items[0].id == "echo" && m.items[0].last)
        );
        press(&mut l.app, "<Esc>");
        assert_eq!(l.app.mode, Mode::Normal);

        // Folders: their own rule (the defaults: enter first).
        l.goto("proj");
        press(&mut l.app, "O");
        assert_eq!(ids(&l.app).first().map(String::as_str), Some("enter"));
        press(&mut l.app, "<CR>");
        l.until(|a| a.cwd.display().ends_with("proj") && a.view_listing.is_some());

        // Locations has nothing to open with.
        press(&mut l.app, "gl");
        press(&mut l.app, "O");
        assert_ne!(l.app.mode, Mode::OpenWith);
        assert!(status(&l.app).contains("O opens files and folders"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn picker_rows(app: &App) -> Vec<String> {
        app.picker
            .as_ref()
            .map(|p| p.view.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The finders: files streamed in by the walker, recent folders, grep re-walked as
    /// you type, and the commands picker running an action.
    #[test]
    fn finders_files_recent_grep_and_commands() {
        let dir = temp_tree();
        let mut l = Live::new(&dir);
        l.cd(&dir.join("src"));

        // Space f f: every file below, streamed; typing filters.
        press(&mut l.app, "<Space>ff");
        assert_eq!(l.app.mode, Mode::Picker);
        l.until(|a| a.picker.as_ref().is_some_and(|p| !p.walking) && picker_rows(a).len() == 3);
        let mut rows = picker_rows(&l.app);
        rows.sort();
        assert_eq!(rows, ["a.txt", "b.txt", r"proj\sub\c.txt"]);
        let screen = render(&mut l.app, 160, 30);
        assert!(
            screen.contains("Files") && screen.contains("scanned"),
            "{screen}"
        );
        assert!(screen.contains("^v reveal"), "{screen}");
        press(&mut l.app, "sub");
        l.until(|a| picker_rows(a) == [r"proj\sub\c.txt"]);
        // Tab chooses, Enter yanks the chosen ones.
        press(&mut l.app, "<C-u>.txt");
        l.until(|a| picker_rows(a).len() == 3);
        press(&mut l.app, "<Tab><Tab><CR>");
        assert_eq!(l.app.mode, Mode::Normal);
        assert_eq!(l.app.register.as_ref().map(|r| r.items.len()), Some(2));
        assert!(status(&l.app).contains("yanked 2"));

        // Space f r: the folders visited (src), then the cwd's subfolders (proj).
        press(&mut l.app, "<Space>fr");
        l.until(|a| picker_rows(a).len() >= 2);
        let rows = picker_rows(&l.app);
        let src = dir.join("src").display().to_string();
        assert!(
            rows.iter().any(|r| r.eq_ignore_ascii_case(&src)),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r.ends_with("proj")), "{rows:?}");
        let proj = rows.iter().position(|r| r.ends_with("proj")).unwrap_or(0);
        if let Some(p) = l.app.picker.as_mut() {
            p.set_cursor(proj);
        }
        press(&mut l.app, "<CR>");
        l.until(|a| a.cwd.display().ends_with("proj"));
        press(&mut l.app, "-");
        l.until(|a| a.cwd.display().eq_ignore_ascii_case(&src) && a.view_listing.is_some());

        // Space /: contents, searched once typing pauses; a new text walks again.
        press(&mut l.app, "<Space>/");
        press(&mut l.app, "bb");
        l.until(|a| picker_rows(a) == ["b.txt"]);
        assert!(
            l.app
                .picker
                .as_ref()
                .is_some_and(|p| p.view[0].detail == "1 match")
        );
        press(&mut l.app, "<BS><BS>c");
        l.until(|a| picker_rows(a) == [r"proj\sub\c.txt"]);
        press(&mut l.app, "<Esc>q");
        assert_eq!(l.app.mode, Mode::Normal);

        // Space :: every action, found by what it does; Enter runs it.
        let hidden = l.app.show_hidden;
        press(&mut l.app, "<Space>:");
        press(&mut l.app, "toggle hidden");
        l.until(|a| picker_rows(a).first().is_some_and(|r| r.contains("hidden")));
        press(&mut l.app, "<CR>");
        assert_eq!(l.app.show_hidden, !hidden);
        // A : command that needs an argument opens the command line for it.
        press(&mut l.app, "<Space>:");
        press(&mut l.app, "rename a tag");
        l.until(|a| !picker_rows(a).is_empty());
        press(&mut l.app, "<CR>");
        assert_eq!(
            l.app.prompt.as_ref().map(|p| (p.kind, p.editor.text())),
            Some((PromptKind::Command, "tag rename ".to_string()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bulk rename panel, as in mockup 12: `r` on a selection, a PowerRename regex,
    /// a collision blocks Enter until that item is excluded, the question, a "no" comes
    /// back to the panel, a "yes" renames on disk; the options are remembered.
    #[test]
    fn bulk_rename_panel_previews_blocks_and_renames() {
        let dir = std::env::temp_dir().join(format!(
            "sentier-rename-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        for f in [
            "B-1001_rev3.pdf",
            "B-1003_rev1.pdf",
            "HIT_B-1003_R1.pdf",
            "notes.txt",
        ] {
            let _ = std::fs::write(dir.join(f), "x");
        }
        let mut l = Live::new(&dir);
        l.goto("B-1001_rev3.pdf");
        press(&mut l.app, "<Tab>");
        l.goto("B-1003_rev1.pdf");
        press(&mut l.app, "<Tab>");
        press(&mut l.app, "r");
        assert_eq!(l.app.mode, Mode::Rename);
        assert_eq!(
            l.app.rename_panel.as_ref().map(|p| p.sources.len()),
            Some(2)
        );

        type_text(&mut l.app, r"(B-\d{4})_rev(\d+)");
        press(&mut l.app, "<Tab>");
        type_text(&mut l.app, "HIT_$1_R$2");
        press(&mut l.app, "<Esc>r");
        let rows = l_plan(&l.app);
        assert_eq!(
            rows,
            [
                ("HIT_B-1001_R3.pdf".to_string(), "ok".to_string()),
                (
                    "HIT_B-1003_R1.pdf".to_string(),
                    "collides with existing HIT_B-1003_R1.pdf".to_string()
                )
            ]
        );
        let screen = render(&mut l.app, 160, 40);
        assert!(
            screen.contains("Bulk rename") && screen.contains("r regex [x]"),
            "{screen}"
        );
        assert!(screen.contains("1 blocked"), "{screen}");
        // Blocked: Enter refuses.
        press(&mut l.app, "<CR>");
        assert_eq!(l.app.mode, Mode::Rename);
        assert!(status(&l.app).contains("blocked"));
        // Exclude it (Space on its row): now it can go.
        press(&mut l.app, "j ");
        press(&mut l.app, "<CR>");
        assert!(
            l.app.confirm_note().contains("Rename 1 item? (1 skipped)"),
            "{}",
            l.app.confirm_note()
        );
        press(&mut l.app, "n");
        assert_eq!(l.app.mode, Mode::Rename, "no: back to the panel");
        press(&mut l.app, "<CR>y");
        assert!(l.app.rename_panel.is_none());
        l.until(|a| a.ops_running == 0 && dir.join("HIT_B-1001_R3.pdf").exists());
        assert!(!dir.join("B-1001_rev3.pdf").exists());
        assert!(dir.join("B-1003_rev1.pdf").exists(), "excluded: untouched");
        // The options are remembered; :rename works on the cursor item.
        l.goto("notes.txt");
        press(&mut l.app, ":rename<CR>");
        assert!(l.app.rename_panel.as_ref().is_some_and(|p| p.options.regex));
        press(&mut l.app, "<Esc>q");
        assert_eq!(l.app.mode, Mode::Normal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "This folder in Explorer" from the `O` menu on a highlighted subfolder shows the
    /// folder you're in, with the subfolder selected (`/select`), not the subfolder
    /// itself; `gx` opens the folder you're in.
    #[test]
    fn explorer_shows_the_folder_on_screen() {
        let dir = temp_tree();
        let mut l = Live::new(&dir.join("src"));
        l.goto("proj");
        press(&mut l.app, "O");
        let pos = l
            .app
            .open_menu
            .as_ref()
            .and_then(|m| m.items.iter().position(|i| i.id == "explorer"));
        let Some(pos) = pos else {
            panic!("no Explorer entry for a folder")
        };
        assert_eq!(
            l.app.open_menu.as_ref().map(|m| m.items[pos].label.clone()),
            Some("This folder in Explorer".to_string())
        );
        if let Some(m) = l.app.open_menu.as_mut() {
            m.cursor = m.shown().iter().position(|&i| i == pos).unwrap_or(0);
        }
        press(&mut l.app, "<CR>");
        l.until(|a| status(a).contains("revealed proj"));
        press(&mut l.app, "gx");
        l.until(|a| status(a).starts_with("Explorer: ") && status(a).ends_with(r"\src"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `g Space` / `:cd` take Windows' variables: `%TEM` + Tab completes the name,
    /// `%TEMP%` and `$env:APPDATA` go there.
    #[test]
    fn cd_expands_environment_variables() {
        let dir = temp_tree();
        let mut l = Live::new(&dir);
        let same = |a: &App, p: &str| {
            a.cwd
                .display()
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(p.trim_end_matches('\\'))
        };
        let temp = std::env::var("TEMP").unwrap_or_default();
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        press(&mut l.app, "g<Space>");
        type_text(&mut l.app, "%TEM");
        press(&mut l.app, "<Tab>");
        assert_eq!(
            l.app.prompt.as_ref().map(|p| p.editor.text()),
            Some(r"cd %TEMP%\".to_string())
        );
        press(&mut l.app, "<CR>");
        l.until(|a| same(a, &temp));
        press(&mut l.app, ":cd $env:APPDATA<CR>");
        l.until(|a| same(a, &appdata));
        press(&mut l.app, r":cd %appdata%\..<CR>");
        l.until(|a| a.cwd.name().eq_ignore_ascii_case("AppData"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `,s` `,S` `,m` `,e` `,n` order the folder (folders first, the cursor stays on its
    /// item), the status bar says how, the choice is saved for next time, and `:sort`
    /// does the same; in search results the same keys sort the results.
    #[test]
    fn comma_keys_sort_the_folder_and_remember_it() {
        let dir = std::env::temp_dir().join(format!(
            "sentier-sort-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("zfolder"));
        let base = std::time::SystemTime::now() - Duration::from_secs(10 * 86_400);
        for (name, size, days) in [("a.txt", 300, 1), ("b.pdf", 10, 5), ("c.dwg", 2000, 3)] {
            let p = dir.join(name);
            let _ = std::fs::write(&p, "x".repeat(size));
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) {
                let _ = f.set_modified(base + Duration::from_secs(days * 86_400));
            }
        }
        let mut l = Live::new(&dir);
        assert_eq!(l.names(), ["zfolder", "a.txt", "b.pdf", "c.dwg"]);
        l.goto("b.pdf");
        press(&mut l.app, ",s");
        assert_eq!(
            l.names(),
            ["zfolder", "c.dwg", "a.txt", "b.pdf"],
            "biggest first"
        );
        assert_eq!(
            l.app.selected().map(|e| e.name.clone()).as_deref(),
            Some("b.pdf"),
            "the cursor stays on its item"
        );
        assert!(
            status(&l.app).contains("biggest first"),
            "{}",
            status(&l.app)
        );
        // Once the message fades, the status line keeps saying how it's sorted.
        l.app.status = None;
        let screen = render(&mut l.app, 160, 20);
        assert!(screen.contains("sort: size ↓"), "{screen}");
        press(&mut l.app, ",S");
        assert_eq!(l.names(), ["zfolder", "b.pdf", "a.txt", "c.dwg"]);
        press(&mut l.app, ",m");
        assert_eq!(
            l.names(),
            ["zfolder", "b.pdf", "c.dwg", "a.txt"],
            "newest first"
        );
        press(&mut l.app, ",e");
        assert_eq!(
            l.names(),
            ["zfolder", "c.dwg", "b.pdf", "a.txt"],
            "dwg pdf txt"
        );
        // Saved for next time.
        let saved = crate::state::path().unwrap_or_default();
        l.until(|_| crate::state::load(&saved).sort == "ext");
        // :sort, and back to name.
        press(&mut l.app, ":sort size<CR>");
        assert_eq!(l.app.sort.key, crate::fs::sort::SortKey::Size);
        press(&mut l.app, ",n");
        assert_eq!(l.names(), ["zfolder", "a.txt", "b.pdf", "c.dwg"]);
        l.app.status = None;
        assert!(render(&mut l.app, 160, 20).contains("sort: name ↑"));
        // ,r outside results explains itself.
        press(&mut l.app, ",r");
        assert!(
            status(&l.app).contains("search results"),
            "{}",
            status(&l.app)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `f` filters this folder with the search's language: the hint says so, `ext:pdf`
    /// keeps PDFs only (not `.pdf.bak`, not a `pdfs` folder), a half-typed query says
    /// why and filters as plain text meanwhile, `Esc` clears.
    #[test]
    fn f_filters_by_extension_glob_and_size() {
        let dir = std::env::temp_dir().join(format!(
            "sentier-filter-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("pdfs"));
        for (f, n) in [
            ("B-1001_rev3.pdf", 10),
            ("B-1002_rev1.PDF", 3000),
            ("report.pdf.bak", 1),
            ("plan.dwg", 2000),
        ] {
            let _ = std::fs::write(dir.join(f), "x".repeat(n));
        }
        let mut l = Live::new(&dir);
        press(&mut l.app, "f");
        assert!(
            l.app
                .prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("ext:pdf") && p.note.contains("*.pdf")),
            "the hint shows what it understands"
        );
        type_text(&mut l.app, "ext:pdf");
        assert_eq!(l.names(), ["B-1001_rev3.pdf", "B-1002_rev1.PDF"]);
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "*.pdf size:>1kb");
        assert_eq!(l.names(), ["B-1002_rev1.PDF"]);
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "pdf");
        assert_eq!(l.names().len(), 4, "plain text: a substring, as always");
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "size:>");
        assert!(
            l.app
                .prompt
                .as_ref()
                .is_some_and(|p| p.note.contains("not a size")),
            "a half-typed filter says why"
        );
        // Enter keeps it; Esc clears it.
        press(&mut l.app, "<C-u>");
        type_text(&mut l.app, "ext:dwg");
        press(&mut l.app, "<CR>");
        assert_eq!(l.names(), ["plan.dwg"]);
        press(&mut l.app, "<Esc>");
        assert_eq!(l.names().len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tabs: `t` opens one on this folder, each keeps its own folder, history and
    /// selection; `Alt-n`, `gt` / `gT`, `{n}gt` switch; `q` closes a tab, then quits;
    /// `:tabnew <path>`; the header lists them.
    #[test]
    fn tabs_keep_their_own_folder_history_and_selection() {
        let dir = std::env::temp_dir().join(format!(
            "sentier-tabs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("alpha"));
        let _ = std::fs::create_dir_all(dir.join("beta"));
        let _ = std::fs::write(dir.join("alpha").join("x.txt"), "x");
        let _ = std::fs::write(dir.join("alpha").join("y.txt"), "y");
        let home = dir.display().to_string().to_lowercase();
        let in_alpha = dir.join("alpha").display().to_string().to_lowercase();
        let at = |a: &App, want: &str| {
            a.cwd.display().to_lowercase() == want && a.view_listing.is_some()
        };
        let mut l = Live::new(&dir);
        assert!(l.app.tab_labels().is_empty(), "no tab bar with one tab");

        // Tab 2 goes into alpha and selects x.txt.
        press(&mut l.app, "t");
        assert_eq!((l.app.tabs.len(), l.app.tab), (2, 1));
        l.goto("alpha");
        press(&mut l.app, "l");
        l.until(|a| at(a, &in_alpha));
        l.goto("x.txt");
        press(&mut l.app, "<Tab>");
        assert_eq!(l.app.marked.len(), 1);

        // Alt-1: tab 1 is still at the top, with no history and no selection.
        press(&mut l.app, "<A-1>");
        l.until(|a| at(a, &home));
        assert_eq!(l.app.tab, 0);
        assert!(l.app.marked.is_empty() && l.app.back.is_empty());
        let screen = render(&mut l.app, 120, 20);
        assert!(
            screen.contains(" 1 sentier-tabs") && screen.contains(" 2 alpha "),
            "{screen}"
        );

        // gt: back in alpha, x.txt still selected, H goes back to the top.
        press(&mut l.app, "gt");
        l.until(|a| at(a, &in_alpha));
        assert!(l.app.marked.contains("x.txt"));
        assert_eq!(
            l.app.selected().map(|e| e.name.clone()).as_deref(),
            Some("y.txt")
        );
        press(&mut l.app, "3gt");
        assert!(status(&l.app).contains("no tab 3"));
        press(&mut l.app, "gT");
        l.until(|a| at(a, &home));

        // :tabnew <path>, then q closes tabs one by one and quits on the last.
        press(&mut l.app, ":tabnew beta<CR>");
        l.until(|a| a.cwd.name() == "beta" && a.view_listing.is_some());
        assert_eq!((l.app.tabs.len(), l.app.tab), (3, 1), "right after tab 1");
        press(&mut l.app, "q");
        l.until(|a| at(a, &in_alpha));
        assert_eq!((l.app.tabs.len(), l.app.tab), (2, 1));
        assert!(l.app.quit.is_none());
        press(&mut l.app, "H");
        l.until(|a| at(a, &home));
        press(&mut l.app, "qq");
        assert!(l.app.quit.is_some(), "the last tab quits");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Slice 3: `s` walks into the selected folder, `^s` saves a preset, the rename is
    /// journaled and `u` puts every name back (folders included), the search lands in the
    /// history (`^p`), and `^o` loads the preset.
    #[test]
    fn bulk_rename_subfolders_presets_history_and_undo() {
        let dir = std::env::temp_dir().join(format!(
            "sentier-rename3-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("sub").join("deep"));
        let _ = std::fs::write(dir.join("sub").join("a.txt"), "a");
        let _ = std::fs::write(dir.join("sub").join("deep").join("b.txt"), "b");
        let _ = std::fs::write(dir.join("c.txt"), "c");
        let names = |d: &std::path::Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        };
        let mut l = Live::new(&dir);
        l.goto("sub");
        press(&mut l.app, "<Tab>");
        l.goto("c.txt");
        press(&mut l.app, "<Tab>");
        press(&mut l.app, "r");
        assert_eq!(l.app.mode, Mode::Rename);
        // The search lands in the history; uppercase everything, subfolders included.
        type_text(&mut l.app, "u");
        press(&mut l.app, "<Tab>");
        type_text(&mut l.app, "u");
        press(&mut l.app, "<Esc>tts");
        l.until(|a| a.rename_panel.as_ref().is_some_and(|p| !p.walking));
        let rows = l_plan(&l.app);
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["SUB", "DEEP", "B.txt", "A.txt", "C.txt"],
            "each folder's contents right after it"
        );
        // ^s: a preset.
        press(&mut l.app, "<C-s>");
        type_text(&mut l.app, "upper");
        press(&mut l.app, "<CR>");
        assert_eq!(l.app.rename_presets.len(), 1);
        assert!(status(&l.app).contains("saved preset upper"));
        press(&mut l.app, "<CR>");
        assert!(l.app.confirm_note().contains("Rename 5 items?"));
        press(&mut l.app, "y");
        l.until(|a| a.ops_running == 0 && names(&dir).contains(&"SUB".to_string()));
        assert_eq!(names(&dir.join("SUB")), ["A.txt", "DEEP"]);
        assert_eq!(names(&dir.join("SUB").join("DEEP")), ["B.txt"]);
        assert_eq!(
            l.app.rename_state.search.first().map(String::as_str),
            Some("u")
        );

        // u: the journal, one question, every name back.
        press(&mut l.app, "u");
        l.until(|a| a.confirm.is_some());
        assert!(
            l.app
                .confirm_note()
                .contains("Undo the last bulk rename (5 items, today"),
            "{}",
            l.app.confirm_note()
        );
        press(&mut l.app, "y");
        l.until(|a| a.ops_running == 0 && names(&dir).contains(&"sub".to_string()));
        assert_eq!(names(&dir), ["c.txt", "sub"]);
        assert_eq!(names(&dir.join("sub")), ["a.txt", "deep"]);
        assert_eq!(names(&dir.join("sub").join("deep")), ["b.txt"]);

        // ^p brings the last search back; ^o loads the preset.
        l.until(|a| {
            a.view_listing
                .as_ref()
                .is_some_and(|li| li.entries.iter().any(|e| e.name == "c.txt"))
        });
        l.goto("c.txt");
        press(&mut l.app, ":rename<CR>");
        press(&mut l.app, "<C-p>");
        assert_eq!(
            l.app.rename_panel.as_ref().map(|p| p.search.text()),
            Some("u".to_string())
        );
        press(&mut l.app, "<C-o>");
        let screen = render(&mut l.app, 160, 40);
        assert!(
            screen.contains("Rename presets") && screen.contains("upper"),
            "{screen}"
        );
        press(&mut l.app, "<CR>");
        assert_eq!(
            l.app.rename_panel.as_ref().map(|p| p.options.format),
            Some(crate::rename::engine::Format::Upper)
        );
        press(&mut l.app, "<Esc>q");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn l_plan(app: &App) -> Vec<(String, String)> {
        use crate::rename::plan::Status;
        let Some(p) = app.rename_panel.as_ref() else {
            return Vec::new();
        };
        let Ok(plan) = &p.plan else { return Vec::new() };
        plan.rows
            .iter()
            .map(|r| {
                let s = match &r.status {
                    Status::Rename => "ok".to_string(),
                    Status::Blocked(w) | Status::Warn(w) => w.clone(),
                    Status::Unchanged => "unchanged".to_string(),
                    Status::Excluded => "excluded".to_string(),
                };
                (r.new_name.clone(), s)
            })
            .collect()
    }
}
