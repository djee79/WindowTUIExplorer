
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
        (pin.name.as_str(), pin.kind, pin.key_str()),
        ("Work dir", PinKind::Folder, Some("a"))
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
    assert_eq!(app.pins.pins[0].key_str(), Some("b"));
    press(&mut app, "'z");
    assert!(status(&app).contains("no pin on 'z"));
}

/// `'d` is Work; giving Hit `'dd` asks Work to move first (a key is a key or starts
/// keys, never both), then `'d` waits for its second letter.
fn work_d_then_hit_dd(app: &mut App) {
    press(app, "md<C-u>Work<CR><CR>");
    press(app, ":cd HIT<CR>:pin<CR><C-u>Hit<CR><CR>dd<CR>");
}

#[test]
fn two_letter_key_moves_the_clashing_one_first() {
    let (mut app, _rt) = loaded();
    work_d_then_hit_dd(&mut app);
    let asked = app.prompt.as_ref().map(|p| (p.kind, p.note.clone()));
    assert!(
        asked
            .as_ref()
            .is_some_and(|(k, n)| *k == keys::PromptKind::KeyMove
                && n.contains("'d (Work) blocks 'dd for Hit")),
        "{asked:?}"
    );
    assert_eq!(
        prompt_text(&app).as_deref(),
        Some("dw"),
        "a free key suggested"
    );
    assert_eq!(app.pins.pins.len(), 1, "nothing saved yet");

    // A key that would clash again is refused, the prompt stays.
    press(&mut app, "<C-u>d<CR>");
    assert!(
        app.prompt
            .as_ref()
            .is_some_and(|p| p.note.contains("clashes with 'dd (Hit)")),
        "{:?}",
        app.prompt.as_ref().map(|p| &p.note)
    );
    press(&mut app, "<C-u>dw<CR>");
    assert!(app.prompt.is_none());
    let keys: Vec<_> = app.pins.pins.iter().map(|p| p.key_str()).collect();
    assert_eq!(keys, [Some("dw"), Some("dd")]);
    assert!(status(&app).contains("'dd → Hit"), "{}", status(&app));
    assert!(status(&app).contains("Work → 'dw"), "{}", status(&app));

    // 'd waits for the second letter; Esc drops it.
    press(&mut app, "'d");
    assert_eq!(app.mark_prefix, Some('d'));
    press(&mut app, "w");
    assert_eq!(app.cwd.display(), r"C:\work");
    press(&mut app, "'dd");
    assert_eq!(app.cwd.display(), r"C:\work\HIT");
    press(&mut app, "'d<Esc>");
    assert_eq!(app.mark_prefix, None);
    assert_eq!(app.cwd.display(), r"C:\work\HIT");
    press(&mut app, "'dx");
    assert!(status(&app).contains("no pin on 'dx"), "{}", status(&app));
}

#[test]
fn esc_on_the_move_saves_nothing() {
    let (mut app, _rt) = loaded();
    work_d_then_hit_dd(&mut app);
    press(&mut app, "<Esc><Esc>");
    assert!(app.prompt.is_none());
    assert_eq!(app.pins.pins.len(), 1);
    assert_eq!(app.pins.pins[0].key_str(), Some("d"), "Work keeps 'd");
    assert_eq!(status(&app), "not pinned");
}

#[test]
fn quote_popup_groups_two_letter_keys() {
    let (mut app, _rt) = loaded();
    work_d_then_hit_dd(&mut app);
    press(&mut app, "<CR>"); // the suggested 'dw
    press(&mut app, "'");
    app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
    let screen = render(&mut app, 120, 20);
    assert!(screen.contains("+Hit · Work"), "{screen}");
    press(&mut app, "d");
    assert!(
        app.which_key_visible(),
        "the popup stays up for the second letter"
    );
    let screen = render(&mut app, 120, 20);
    assert!(screen.contains("'d"), "{screen}");
    assert!(
        screen.contains("Hit") && screen.contains("Work"),
        "{screen}"
    );
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
            .is_some_and(|p| p.note.contains("letters or digits")),
        "stays open with a note"
    );
    press(&mut app, "<C-u>n<CR>");
    assert!(app.prompt.is_none());
    let pin = &app.pins.pins[0];
    assert_eq!((pin.kind, pin.key_str()), (PinKind::File, Some("n")));
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
    assert_eq!(app.pins.pins[1].key_str(), Some("x"));

    settle(&mut app);
    press(&mut app, "<C-x>n");
    assert_eq!(app.pins.pins.len(), 2, "n keeps it");
    assert_eq!(app.mode, Mode::Picker);
    // Normal mode: D and dd, the file view's delete keys, ask the same question.
    settle(&mut app);
    press(&mut app, "<Esc>Dn");
    assert_eq!(app.pins.pins.len(), 2, "D asks first");
    assert_eq!(app.mode, Mode::Picker);
    settle(&mut app);
    press(&mut app, "<Esc>ddy");
    assert_eq!(app.pins.pins.len(), 1, "dd deletes after a y");
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
        (v.name.as_str(), v.query.as_str(), v.key_str()),
        ("main server", "#servers", Some("v"))
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

/// `:pins restore` with backups: a picker of them, the preview's difference, and a
/// question that says what changes before anything is touched. (The file swap
/// itself is tested in `pin_backups`, away from the store the app tests share.)
#[test]
fn pins_restore_lists_backups_and_asks_first() {
    let (mut app, _rt) = two_pins();
    let at =
        |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap_or_default();
    let diff = crate::pin_backups::Diff {
        pins: 3,
        now: 2,
        back: vec!["Old share".into()],
        gone: vec![],
        changed: vec![],
    };
    let listed = |when: &str, diff| crate::pin_backups::Listed {
        backup: crate::pin_backups::Backup {
            path: std::path::PathBuf::from(r"C:\nowhere\pins-x.toml"),
            taken: at(when),
        },
        diff,
    };
    app.handle(AppEvent::PinBackups(vec![
        listed("2026-11-09 20:00", Ok(diff)),
        listed("2026-11-08 09:00", Err("bad toml".into())),
    ]));
    settle(&mut app);
    assert_eq!(app.mode, Mode::Picker);
    let rows: Vec<String> = app
        .picker
        .as_ref()
        .map(|p| p.view.iter().map(|i| i.detail.clone()).collect())
        .unwrap_or_default();
    assert_eq!(rows, ["3 pins · 1 back", "doesn't load: bad toml"]);
    press(&mut app, "<CR>");
    assert!(
        prompt_note(&app).contains("Restore the pins from 2026-11-09 20:00")
            && prompt_note(&app).contains("1 back")
            && prompt_note(&app).contains("backed up first"),
        "{}",
        prompt_note(&app)
    );
    press(&mut app, "n");
    assert_eq!(app.pins.pins.len(), 2, "no: nothing changed");

    // A backup that doesn't load can't be chosen.
    app.handle(AppEvent::PinBackups(vec![listed(
        "2026-11-08 09:00",
        Err("bad toml".into()),
    )]));
    settle(&mut app);
    press(&mut app, "<CR>");
    assert!(status(&app).contains("doesn't load"), "{}", status(&app));
    // None yet: says where they'll be.
    press(&mut app, "<Esc>q");
    app.handle(AppEvent::PinBackups(Vec::new()));
    assert!(status(&app).contains("no pin backups yet"));
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
        Some("#aveva,servers "),
        "both chosen tags, any of them"
    );
    settle(&mut app);
    let mut names = picker_names(&app);
    names.sort();
    assert_eq!(names, ["HIT", "Work dir"]);
    press(&mut app, "<Esc>q");
    // Typed by hand, `#a #b` still means all of them.
    press(&mut app, "<Space>p#aveva #servers ");
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
    assert_eq!(app.pins.pins[0].key_str(), Some("w"));
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
        (g.kind, g.url.as_deref(), g.browser.as_deref(), g.key_str()),
        (
            PinKind::Web,
            Some("https://grafana.example/d/x"),
            Some("chrome"),
            Some("g")
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
