
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
    assert_eq!(pin.key_str(), Some("j"));

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
    assert_eq!(pin.key_str(), Some("p"));
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
