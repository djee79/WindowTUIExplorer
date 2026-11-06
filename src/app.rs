//! App state, the UI event loop, and key dispatch.
//!
//! This module never touches the filesystem. It asks the [`Worker`] for things and reacts
//! to the [`AppEvent`]s that come back.

use crate::event::{AppEvent, EventRx};
use crate::fs::path::Loc;
use crate::fs::worker::{ListError, Request, Worker};
use crate::fs::{Entry, Listing};
use crate::platform::DiskSpace;
use crate::ui;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

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
    Filter,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quit {
    Plain,
    /// `Q`: write the cwd to `--cwd-file`.
    WithCwd,
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
    pub filter: String,
    pub mode: Mode,
    pending_g: bool,

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
            filter: String::new(),
            mode: Mode::Normal,
            pending_g: false,
            status: None,
            space: None,
            space_for: None,
            preview_due: None,
            dirty: true,
            quit: None,
            started: Instant::now(),
            last_spin_frame: 0,
        };
        app.set_cwd(start, None);
        app
    }

    // ---------------------------------------------------------------- queries

    pub fn selected(&self) -> Option<&Entry> {
        let listing = self.view_listing.as_ref()?;
        listing.entries.get(*self.view.get(self.cursor)?)
    }

    /// The location shown in the preview column, if the selection is a container.
    pub fn preview_loc(&self) -> Option<Loc> {
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
        !self.pending.is_empty()
    }

    pub fn dir(&self, loc: &Loc) -> Option<&DirState> {
        self.dirs.get(loc)
    }

    // ------------------------------------------------------------- navigation

    fn set_cwd(&mut self, loc: Loc, select: Option<String>) {
        self.remember_cursor();
        self.cwd = loc;
        self.filter.clear();
        if self.mode == Mode::Filter {
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
            self.set_cwd(parent, Some(name));
        }
    }

    fn open_selected(&mut self) {
        let Some(entry) = self.selected() else { return };
        let (name, container) = (entry.name.clone(), entry.is_container());
        match self.cwd.join(&name) {
            Ok(target) if container => self.set_cwd(target, None),
            Ok(target) => {
                self.set_status(format!("opening {name}…"), false);
                self.worker.open(target);
            }
            Err(e) => self.set_status(e.to_string(), true),
        }
    }

    fn go_home(&mut self) {
        let home = std::env::var("USERPROFILE").map_err(|e| e.to_string());
        match home.and_then(|h| Loc::parse(&h).map_err(|e| e.to_string())) {
            Ok(loc) => self.set_cwd(loc, None),
            Err(e) => self.set_status(format!("no home directory: {e}"), true),
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
        if self.pending.contains_key(&loc) {
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
        let req = self.worker.list(loc.clone());
        self.pending.insert(loc, req);
    }

    /// Navigating away cancels in-flight requests for directories no longer on screen.
    fn cancel_unneeded(&mut self) {
        let mut keep: HashSet<Loc> = HashSet::from([self.cwd.clone()]);
        keep.extend(self.cwd.parent());
        keep.extend(self.preview_loc());
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
        let Some(volume) = self.cwd.as_fs().map(|p| Loc::Fs(p.volume_root())) else {
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
        self.preview_due = Some(Instant::now() + PREVIEW_DELAY);
    }

    fn refresh(&mut self) {
        self.request(self.cwd.clone(), true);
        if let Some(p) = self.preview_loc() {
            self.request(p, true);
        }
        self.request_space(true);
        self.set_status("refreshing…".to_string(), false);
    }

    // ------------------------------------------------------------------ views

    /// Rebuilds the visible cwd rows from its listing, keeping the selection by name.
    fn rebuild_view(&mut self) {
        let keep = self
            .selected()
            .map(|e| e.name.clone())
            .or_else(|| self.remembered.get(&self.cwd).cloned());
        let listing = self.dirs.get(&self.cwd).and_then(|s| s.listing.clone());
        self.view = match &listing {
            Some(l) => visible_indices(l, self.show_hidden, &self.filter),
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
            AppEvent::Term(Event::Resize(..)) => self.dirty = true,
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
        }
    }

    fn on_listed(&mut self, id: u64, loc: Loc, result: Result<Arc<Listing>, ListError>) {
        if self.pending.get(&loc).is_none_or(|r| r.id != id) {
            return; // cancelled or superseded
        }
        self.pending.remove(&loc);
        let state = self.dirs.entry(loc.clone()).or_default();
        state.loading = None;
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
        }
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
        if self.preview_due.is_some_and(|due| now >= due) {
            self.preview_due = None;
            if let Some(p) = self.preview_loc() {
                self.request(p, false);
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
        if self.is_busy() {
            wake = wake.min(SPINNER_TICK);
        }
        if let Some(due) = self.preview_due {
            wake = wake.min(due.saturating_duration_since(now));
        }
        wake
    }

    // ------------------------------------------------------------------- keys

    fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = Some(Quit::Plain);
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Normal,
            Mode::Filter => self.on_filter_key(key),
            Mode::Normal => self.on_normal_key(key, ctrl),
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.rebuild_view();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                if self.filter.pop().is_none() {
                    self.mode = Mode::Normal;
                }
                self.rebuild_view();
            }
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.cursor = 0;
                self.rebuild_view();
            }
            _ => {}
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent, ctrl: bool) {
        if std::mem::take(&mut self.pending_g) {
            match key.code {
                KeyCode::Char('g') => self.set_cursor(0),
                KeyCode::Char('l') => self.set_cwd(Loc::Root, None),
                _ => {}
            }
            return;
        }
        let half = (self.page / 2).max(1) as isize;
        match key.code {
            KeyCode::Char('d') if ctrl => self.move_cursor(half),
            KeyCode::Char('u') if ctrl => self.move_cursor(-half),
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => self.go_parent(),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => self.open_selected(),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('G') | KeyCode::End => self.set_cursor(usize::MAX),
            KeyCode::Home => self.set_cursor(0),
            KeyCode::PageDown => self.move_cursor(self.page.max(1) as isize),
            KeyCode::PageUp => self.move_cursor(-(self.page.max(1) as isize)),
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.rebuild_view();
                let state = if self.show_hidden { "shown" } else { "hidden" };
                self.set_status(format!("hidden/system files {state}"), false);
            }
            KeyCode::Char('~') => self.go_home(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_view();
            }
            KeyCode::Char('R') => self.refresh(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('q') => self.quit = Some(Quit::Plain),
            KeyCode::Char('Q') => self.quit = Some(Quit::WithCwd),
            _ => {}
        }
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
    loop {
        let now = Instant::now();
        app.tick(now);
        if app.dirty && now.duration_since(last_draw) >= FRAME {
            terminal.draw(|f| ui::draw(f, app))?;
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

    fn entry(name: &str, kind: EntryKind, size: u64, hidden: bool) -> Entry {
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
        }
    }

    fn listing(mut entries: Vec<Entry>) -> Arc<Listing> {
        sort_entries(&mut entries);
        Arc::new(Listing {
            entries,
            loaded_at: Instant::now(),
        })
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
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
    fn fixture() -> (App, tokio::runtime::Runtime) {
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

    #[test]
    fn keys_navigate_filter_and_toggle_hidden() {
        let (mut app, _rt) = fixture();
        let key = |c: KeyCode| AppEvent::Term(Event::Key(KeyEvent::from(c)));

        app.handle(key(KeyCode::Char('G')));
        assert_eq!(app.selected().map(|e| e.name.as_str()), Some("spec10.xlsx"));

        app.handle(key(KeyCode::Char('.')));
        assert_eq!(app.view.len(), 5, "hidden shown");
        assert_eq!(
            app.selected().map(|e| e.name.as_str()),
            Some("spec10.xlsx"),
            "selection kept by name across view rebuilds"
        );

        for k in [KeyCode::Char('/'), KeyCode::Char('x'), KeyCode::Char('l')] {
            app.handle(key(k));
        }
        assert_eq!(app.view.len(), 2, "filter 'xl' matches the two specs");
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.view.len(), 5, "Esc clears the filter");

        app.handle(key(KeyCode::Char('g')));
        app.handle(key(KeyCode::Char('g')));
        assert_eq!(app.selected().map(|e| e.name.as_str()), Some("HIT"));

        // Enter HIT (cached), then go back up: cursor returns to HIT.
        app.handle(key(KeyCode::Char('l')));
        assert_eq!(app.cwd.display(), r"C:\work\HIT");
        app.handle(key(KeyCode::Char('h')));
        assert_eq!(app.cwd.display(), r"C:\work");
        assert_eq!(app.selected().map(|e| e.name.as_str()), Some("HIT"));

        app.handle(key(KeyCode::Char('Q')));
        assert_eq!(app.quit, Some(Quit::WithCwd));
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
