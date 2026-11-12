
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
fn a_single_step_previews_at_once_a_stream_waits() {
    let (mut app, _rt) = fixture();
    // The cursor has been resting: the preview of the next row is due now.
    app.moved_at = Instant::now() - Duration::from_secs(1);
    press(&mut app, "j");
    let now = Instant::now();
    assert!(app.preview_due.is_some_and(|due| due <= now), "at once");
    assert!(app.image_due <= now, "the picture too");
    // The next move follows within a key repeat: wait for the cursor to stop.
    press(&mut app, "j");
    let now = Instant::now();
    assert!(app.preview_due.is_some_and(|due| due > now), "debounced");
    assert!(app.image_due > now);
    assert!(app.next_wake(now) <= PREVIEW_DELAY, "and woken for it");
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

/// The rename presets and history are read at startup off-thread; a read that lands
/// after a preset was saved or a rename made merges with them instead of erasing
/// them (it made a test flaky once).
#[test]
fn a_late_rename_store_read_keeps_this_sessions_work() {
    use crate::rename::presets::{Preset, State};
    let (mut app, _rt) = fixture();
    app.rename_presets = vec![Preset {
        name: "mine".into(),
        ..Preset::default()
    }];
    app.rename_state.search = vec!["new".into()];
    app.handle(AppEvent::RenameStore {
        presets: Ok(vec![
            Preset {
                name: "saved".into(),
                ..Preset::default()
            },
            Preset {
                name: "MINE".into(),
                search: "old".into(),
                ..Preset::default()
            },
        ]),
        state: Box::new(State {
            search: vec!["old".into(), "new".into()],
            ..State::default()
        }),
    });
    let names: Vec<&str> = app.rename_presets.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["mine", "saved"],
        "this session's preset wins its name"
    );
    assert_eq!(app.rename_state.search, ["new", "old"]);
    // Nothing done yet: the saved state is taken as is.
    let (mut fresh, _rt2) = fixture();
    fresh.handle(AppEvent::RenameStore {
        presets: Ok(Vec::new()),
        state: Box::new(State {
            search: vec!["old".into()],
            ..State::default()
        }),
    });
    assert_eq!(fresh.rename_state.search, ["old"]);
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
fn locations_have_the_relative_number_gutter() {
    let (mut app, _rt) = root_fixture();
    let line_of = |screen: &str, what: &str| {
        screen
            .lines()
            .find(|l| l.contains(what))
            .unwrap_or_default()
            .to_string()
    };
    let screen = render(&mut app, 160, 24);
    // The cursor row: its absolute number, left-aligned; the others: the distance,
    // counting entries only (the section headers in between don't count).
    assert!(
        line_of(&screen, "C:  Windows").contains("│1     ● C:"),
        "{screen}"
    );
    assert!(
        line_of(&screen, "archive                            timed out").contains("│  2   × "),
        "{screen}"
    );
    assert!(!line_of(&screen, "DRIVES").contains('1'), "{screen}");
    press(&mut app, "2j");
    let screen = render(&mut app, 160, 24);
    assert!(
        line_of(&screen, "archive                            timed out").contains("│3     × "),
        "{screen}"
    );
    assert!(
        line_of(&screen, "C:  Windows").contains("│  2   ● C:"),
        "{screen}"
    );
    press(&mut app, ":set nonu<CR>:set nornu<CR>");
    let screen = render(&mut app, 160, 24);
    assert!(
        line_of(&screen, "C:  Windows").contains("│  ● C:"),
        "{screen}"
    );
}

/// Locations with these entries (in order), drawn.
fn locations_screen(entries: Vec<Entry>) -> String {
    let (mut app, _rt) = fixture();
    app.dirs.insert(
        Loc::Root,
        DirState {
            listing: Some(listing_unsorted(entries)),
            ..DirState::default()
        },
    );
    app.set_cwd(Loc::Root, None);
    render(&mut app, 160, 24)
}

#[test]
fn shares_hint_only_when_there_are_none() {
    let share = || Entry::virtual_entry(r"\\srv01\project".into(), EntryKind::Share, "".into());
    let machine =
        || Entry::virtual_entry("PC-042".into(), EntryKind::Machine(Some(true)), "".into());
    let hint = "none yet: browse a";
    // Shares and machines: MACHINES is the last section, still no hint.
    let screen = locations_screen(vec![share(), machine()]);
    assert!(
        screen.contains("MACHINES") && !screen.contains(hint),
        "{screen}"
    );
    // Machines only: the hint, above MACHINES.
    let screen = locations_screen(vec![machine()]);
    let (at_hint, at_machines) = (screen.find(hint), screen.find("MACHINES"));
    assert!(at_hint.is_some() && at_hint < at_machines, "{screen}");
    // Nothing at all: the hint.
    assert!(locations_screen(Vec::new()).contains(hint));
}

#[test]
fn which_key_dd_in_locations_says_files_are_untouched() {
    let (mut app, _rt) = root_fixture();
    let popup = |app: &mut App| {
        press(app, "d");
        app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
        let screen = render(app, 160, 24);
        press(app, "<Esc>");
        screen
    };
    let screen = popup(&mut app);
    assert!(screen.contains("drives can't be removed"), "{screen}");
    assert!(!screen.contains("Recycle Bin"), "{screen}");
    press(&mut app, "2j");
    let screen = popup(&mut app);
    assert!(
        screen.contains("forget this share (files untouched)"),
        "{screen}"
    );
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
