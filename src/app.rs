//! App state and the UI event loop. Key dispatch, actions and prompts live in
//! [`keys`].
//!
//! This module never touches the filesystem. It asks the [`Worker`] for things and reacts
//! to the [`AppEvent`]s that come back.

pub mod auth;
mod keys;
mod picker;
mod pins;
mod preview;
mod search;
mod tags;

pub use keys::{Prompt, PromptKind};
pub use search::SearchState;

use crate::event::{AppEvent, EventRx};
use crate::fs::path::{FsPath, Loc};
use crate::fs::worker::{ListError, Probe, Request, Worker};
use crate::fs::{Entry, EntryKind, Listing};
use crate::keymap::{Key, Keymap};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quit {
    Plain,
    /// `ZZ` / `:wq`: write the cwd to `--cwd-file`.
    WithCwd,
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
    connections_made: HashSet<String>,
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
            searches: Vec::new(),
            next_search_id: 1,
            grep_preview: None,
            shares: Vec::new(),
            shares_loaded: false,
            pins: crate::pins::PinStore::default(),
            pins_loaded: false,
            pin_draft: None,
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
            connections_made: HashSet::new(),
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
        app.worker.load_keymap();
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
        !self.pending.is_empty() || self.searching() || self.image_busy()
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
        self.offset = 0;

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
        if let Some(parent) = self.cwd.parent() {
            let name = self.cwd.name();
            self.navigate(parent, Some(name));
        }
    }

    fn open_selected(&mut self) {
        let Some(entry) = self.selected() else { return };
        let (name, container) = (entry.name.clone(), entry.is_container());
        // A Locations entry known to be unreachable: say so and stay put (never hang),
        // and probe again so a second Enter works once it's back.
        if self.at_root()
            && let Ok(target) = self.cwd.join(&name)
            && let Some(Err(e)) = self.reach.get(&target).and_then(|r| r.last.clone())
        {
            self.set_status(format!("{name} is {e}; probing again…"), true);
            self.probe(target);
            return;
        }
        match self.cwd.join(&name) {
            Ok(target) if container => self.navigate(target, None),
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
        let state = self.dirs.entry(loc.clone()).or_default();
        if !force && (state.listing.is_some() || state.error.is_some()) {
            return;
        }
        state.loading = Some(Loading {
            since: Instant::now(),
            count: 0,
        });
        let req = match loc {
            Loc::Root => self
                .worker
                .list_root(self.shares.clone(), self.pin_entries()),
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
            .selected()
            .map(|e| e.name.clone())
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
        self.view_listing = listing;
        let found = keep.and_then(|name| {
            let entries = &self.view_listing.as_ref()?.entries;
            self.view
                .iter()
                .position(|&i| entries[i].name.eq_ignore_ascii_case(&name))
        });
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
                Ok(config) => self.config = config,
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
            } => self.on_search_progress(id, hits, scanned, current),
            AppEvent::SearchDone {
                id,
                scanned,
                errors,
                elapsed,
                cancelled,
            } => self.on_search_done(id, scanned, errors, elapsed, cancelled),
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
        if self.is_busy() || self.picker.as_ref().is_some_and(|p| p.running) {
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

/// Indices of the entries to show, after the hidden toggle and a case-insensitive
/// substring filter.
pub fn visible_indices(listing: &Listing, show_hidden: bool, filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    listing
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| show_hidden || !e.is_hidden())
        .filter(|(_, e)| needle.is_empty() || e.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// The UI loop: drain events, tick, draw when dirty (capped at ~60 fps), sleep until the
/// next event or the next time-driven change. Returns how the user quit.
pub fn run(terminal: &mut DefaultTerminal, app: &mut App, rx: &EventRx) -> anyhow::Result<Quit> {
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
            if k.shift {
                mods |= KeyModifiers::SHIFT;
            }
            app.handle(AppEvent::Term(Event::Key(KeyEvent::new(k.code, mods))));
        }
    }

    fn sel(app: &App) -> Option<&str> {
        app.selected().map(|e| e.name.as_str())
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
        press(&mut app, ":sync<CR>");
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("arrives in M4b"))
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
