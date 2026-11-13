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

fn listed(app: &App, name: &str) -> bool {
    app.view_listing
        .as_ref()
        .is_some_and(|l| l.entries.iter().any(|e| e.name == name))
}

#[test]
fn zip_and_extract_from_the_keys() {
    let dir = temp_tree();
    let src = dir.join("src");
    let mut l = Live::new(&src);

    // <leader>zc on a folder: the name suggested, Enter zips it as a job.
    l.goto("proj");
    press(&mut l.app, "<Space>zc");
    let asked = l.app.prompt.as_ref().map(|p| (p.kind, p.editor.text()));
    assert_eq!(asked, Some((PromptKind::ZipName, "proj.zip".into())));
    assert!(
        l.app
            .prompt
            .as_ref()
            .is_some_and(|p| p.note == "proj → proj.zip"),
        "{:?}",
        l.app.prompt.as_ref().map(|p| &p.note)
    );
    press(&mut l.app, "<CR>");
    assert_eq!(l.app.jobs.len(), 1);
    l.until(|a| a.jobs.iter().all(|j| j.phase.is_over()) && listed(a, "proj.zip"));
    assert_eq!(l.app.jobs[0].phase, Phase::Done, "{}", status(&l.app));
    assert!(
        status(&l.app).contains("done: proj.zip · 3 entries · "),
        "{}",
        status(&l.app)
    );

    // <leader>zx on it: its folder is proj (named by the zip), which exists: asked.
    l.goto("proj.zip");
    press(&mut l.app, "<Space>zx");
    l.until(|a| a.prompt.is_some());
    let d = l.app.dialog_view();
    assert!(
        d.as_ref()
            .is_some_and(|d| d.buttons == ["Merge", "New folder"]
                && !d.yes
                && d.message.starts_with("proj already exists here")
                && d.message.contains("new folder proj_1")),
        "{d:?}"
    );
    // Esc: nothing.
    press(&mut l.app, "<Esc>");
    assert_eq!(status(&l.app), "not extracted");
    assert_eq!(l.app.jobs.len(), 1);
    // n: a new folder, proj_1.
    press(&mut l.app, "<Space>zx");
    l.until(|a| a.prompt.is_some());
    press(&mut l.app, "n");
    l.until(|a| a.jobs.iter().all(|j| j.phase.is_over()) && listed(a, "proj_1"));
    assert_eq!(
        std::fs::read_to_string(src.join(r"proj_1\sub\c.txt"))
            .ok()
            .as_deref(),
        Some("c")
    );
    assert!(
        status(&l.app).contains("extracted to proj_1"),
        "{}",
        status(&l.app)
    );
    // m: into proj itself; a file of mine there stays, no proj_2.
    let _ = std::fs::write(src.join(r"proj\mine.txt"), "mine");
    let jobs_before = l.app.jobs.len();
    l.goto("proj.zip");
    press(&mut l.app, "<Space>zx");
    l.until(|a| a.prompt.is_some());
    press(&mut l.app, "m");
    l.until(|a| a.jobs.len() > jobs_before && a.jobs.iter().all(|j| j.phase.is_over()));
    assert!(
        status(&l.app).contains("extracted to proj ·"),
        "{}",
        status(&l.app)
    );
    assert!(src.join(r"proj\mine.txt").exists() && !src.join("proj_2").exists());

    // :zip with a name, on a selection; the name gets its .zip.
    l.goto("a.txt");
    press(&mut l.app, "<Tab><Tab>:zip two<CR>");
    l.until(|a| a.jobs.iter().all(|j| j.phase.is_over()) && listed(a, "two.zip"));
    assert!(l.app.marked.is_empty(), "the selection is used up");

    // Not an archive / not a folder: said, nothing started.
    let jobs = l.app.jobs.len();
    l.goto("a.txt");
    press(&mut l.app, "<Space>zx");
    assert!(
        status(&l.app).contains("isn't an archive"),
        "{}",
        status(&l.app)
    );
    l.goto("proj");
    press(&mut l.app, "<Space>zx");
    assert!(status(&l.app).contains("put the cursor on an archive"));
    // A .7z without 7-Zip.
    l.app.seven_zip = None;
    let _ = std::fs::write(src.join("x.7z"), "x");
    press(&mut l.app, "R");
    l.until(|a| listed(a, "x.7z"));
    l.goto("x.7z");
    press(&mut l.app, "<Space>zx");
    assert!(status(&l.app).contains("needs 7-Zip"), "{}", status(&l.app));
    assert_eq!(l.app.jobs.len(), jobs);
    // Esc on the name prompt zips nothing.
    press(&mut l.app, "<Space>zc<Esc><Esc>");
    assert_eq!(status(&l.app), "not zipped");
    assert_eq!(l.app.jobs.len(), jobs);
    let _ = std::fs::remove_dir_all(&dir);
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
    // A cut pasted where it is: nothing to do.
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

/// `yy` then `p` in the same folder: a copy beside the original, numbered like Yazi
/// (`a_1.txt`, then `a_2.txt`), folders too; files and folders together are two jobs.
#[test]
fn paste_where_the_original_is_makes_a_numbered_copy() {
    let dir = temp_tree();
    let src = dir.join("src");
    let mut l = Live::new(&src);
    let has = |name: &'static str| {
        move |a: &App| {
            a.view_listing
                .as_ref()
                .is_some_and(|li| li.entries.iter().any(|e| e.name == name))
        }
    };
    l.goto("a.txt");
    press(&mut l.app, "yyp");
    l.until(|a| a.jobs.first().is_some_and(|j| j.phase.is_over()));
    assert_eq!(
        l.app.jobs[0].phase,
        Phase::Done,
        "{:?}",
        l.app.jobs[0].error
    );
    assert_eq!(
        std::fs::read_to_string(src.join("a_1.txt")).ok().as_deref(),
        Some("a")
    );
    // The listing refreshes, so the next one takes the next number.
    l.until(has("a_1.txt"));
    press(&mut l.app, "p");
    l.until(|a| a.jobs.len() == 2 && a.jobs[1].phase.is_over());
    assert!(src.join("a_2.txt").exists());

    // A folder and a file at once: proj_1 (robocopy) and b_1.txt (native).
    l.until(has("a_2.txt"));
    l.goto("proj");
    press(&mut l.app, "<Tab>");
    l.goto("b.txt");
    press(&mut l.app, "<Tab>yyp");
    l.until(|a| a.jobs.len() == 4 && a.jobs.iter().all(|j| j.phase.is_over()));
    assert!(src.join("proj_1").join("sub").join("c.txt").exists());
    assert!(src.join("b_1.txt").exists());
    assert!(src.join("proj").exists() && src.join("b.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
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

    // Again: both names are there now, so p asks. Esc pastes nothing; s skips them
    // (nothing left); Enter keeps both, under free names.
    press(&mut l.app, "p");
    assert_eq!(l.app.mode, Mode::Prompt);
    let screen = render(&mut l.app, 140, 40);
    assert!(
        screen.contains("2 of 2 names are already in dst")
            && screen.contains("a.txt → a_1.txt")
            && screen.contains("Keep both"),
        "{screen}"
    );
    press(&mut l.app, "<Esc>");
    assert_eq!(l.app.mode, Mode::Normal);
    assert_eq!(status(&l.app), "nothing pasted");
    press(&mut l.app, "ps");
    assert!(
        status(&l.app).contains("all 2 are already in dst"),
        "{}",
        status(&l.app)
    );
    assert_eq!(l.app.jobs.len(), 1);
    press(&mut l.app, "p<CR>");
    l.until(|a| a.jobs.len() == 2 && a.jobs[1].phase.is_over());
    assert!(status(&l.app).contains("2 copied"), "{}", status(&l.app));
    assert_eq!(
        std::fs::read_to_string(dir.join("dst").join("b_1.txt"))
            .ok()
            .as_deref(),
        Some("bb")
    );
    l.until(|a| a.view.len() == 4);
    // o overwrites, without asking a second time.
    std::fs::write(dir.join("dst").join("b.txt"), "old").expect("write");
    press(&mut l.app, "po");
    l.until(|a| a.jobs.len() == 3 && a.jobs[2].phase.is_over());
    assert_eq!(l.app.mode, Mode::Normal);
    assert_eq!(
        std::fs::read_to_string(dir.join("dst").join("b.txt"))
            .ok()
            .as_deref(),
        Some("bb")
    );
    l.app.jobs.pop();

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
            message:
                "The process cannot access the file because it is being used by another process."
                    .into(),
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

/// `^o` / `O` on a sync preset in a picker: a menu with its two folders. "Both" goes to
/// the source and opens the destination in a new tab; `O` in the jobs panel offers a
/// job's folders.
#[test]
fn a_presets_folders_open_from_the_picker_and_the_jobs_panel() {
    let dir = temp_tree();
    let mut l = Live::new(&dir);
    l.until(|a| a.pins_loaded);
    let (src, dst) = (dir.join("src"), dir.join("dst"));
    let text = format!(
        "[[pin]]\nkind = 'sync'\nname = 'proj sync'\nsource = '{}'\ndest = '{}'\n",
        src.display(),
        dst.display()
    );
    let Ok((store, _)) = crate::pins::parse(&text) else {
        panic!("{text}")
    };
    l.app.pins = store;
    let ids = |a: &App| -> Vec<String> {
        a.open_menu
            .as_ref()
            .map(|m| m.items.iter().map(|i| i.id.clone()).collect())
            .unwrap_or_default()
    };
    press(&mut l.app, "<Space>rs");
    l.until(|a| a.picker.as_ref().is_some_and(|p| !p.view.is_empty()));
    press(&mut l.app, "<C-o>");
    assert_eq!(l.app.mode, Mode::OpenWith);
    assert_eq!(
        ids(&l.app),
        [
            "go-src", "go-dst", "both", "x-src", "x-dst", "x-both", "c-src", "c-dst"
        ]
    );
    let screen = render(&mut l.app, 140, 40);
    for want in [
        "Open proj sync",
        "in sentier",
        "in Explorer",
        "copy the path",
    ] {
        assert!(screen.contains(want), "{want}\n{screen}");
    }
    assert!(
        !screen.contains(" with "),
        "not an open-with menu: {screen}"
    );
    // 3: both, the destination in a new tab.
    press(&mut l.app, "3");
    l.until(|a| {
        a.cwd
            .display()
            .eq_ignore_ascii_case(&dst.display().to_string())
    });
    assert_eq!(l.app.tabs.len(), 2);
    press(&mut l.app, "<A-1>");
    l.until(|a| {
        a.cwd
            .display()
            .eq_ignore_ascii_case(&src.display().to_string())
    });

    // The jobs panel: O on a job offers its folders.
    let plan = crate::jobs::Plan {
        backend: crate::jobs::Backend::Robocopy,
        steps: vec![crate::jobs::robocopy::Step {
            src: src.display().to_string(),
            dst: dst.join("src").display().to_string(),
            files: vec![],
        }],
        pairs: vec![],
        title: "src → dst".into(),
    };
    let job = crate::jobs::Job::new(
        42,
        &plan,
        crate::jobs::Op::Copy,
        dst.join("src").display().to_string(),
        crate::jobs::robocopy::Overwrite::Never,
        "x.log".into(),
    );
    l.app.jobs.push(job);
    press(&mut l.app, "J");
    press(&mut l.app, "O");
    assert_eq!(l.app.mode, Mode::OpenWith);
    assert_eq!(
        l.app.open_menu.as_ref().and_then(|m| m.places.clone()),
        Some(super::places::Places {
            source: src.display().to_string(),
            dest: dst.join("src").display().to_string(),
        })
    );
    press(&mut l.app, "<Esc>");
    let _ = std::fs::remove_dir_all(&dir);
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
        (pin.name.as_str(), pin.key_str(), pin.tags.clone()),
        ("src → dst", Some("z"), vec!["aveva".to_string()])
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
    assert_eq!(l.app.pins.pins[0].key_str(), Some("z"), "key kept");

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
    // The zip name: the default, with what it makes now; a bad token is refused.
    assert_eq!(line_now(&l.app), crate::jobs::zip::DEFAULT_FILE_NAME);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert!(
        prompt_note(&l.app).starts_with(&format!("→ src_{today}_")),
        "{}",
        prompt_note(&l.app)
    );
    press(&mut l.app, "<C-u>");
    type_text(&mut l.app, "{name}_{who}");
    assert!(prompt_note(&l.app).contains("unknown {who}"));
    press(&mut l.app, "<CR>");
    assert_eq!(line_now(&l.app), "{name}_{who}", "stays open");
    press(&mut l.app, "<C-u>");
    type_text(&mut l.app, "{name}_{host}_{date:%Y%m%d}");
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    assert!(
        prompt_note(&l.app).starts_with(&format!("→ src_{host}_")),
        "{}",
        prompt_note(&l.app)
    );
    press(&mut l.app, "<CR>");
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
    assert_eq!(b.file_name, "{name}_{host}_{date:%Y%m%d}");

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
    // Notepad++ and VS Code show only where they're installed: not part of the check.
    let ids = |app: &App| -> Vec<String> {
        app.open_menu
            .as_ref()
            .map(|m| {
                m.items
                    .iter()
                    .map(|i| i.id.clone())
                    .filter(|id| id != "notepadpp" && id != "vscode")
                    .collect()
            })
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
            "powershell-here",
            "compress",
            "assoc:np",
            "openwith",
            "properties"
        ]
    );
    let screen = render(&mut l.app, 140, 40);
    assert!(screen.contains("Open a.txt with"), "{screen}");
    assert!(screen.contains("Compress to zip"), "{screen}");
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
    assert_eq!(ids(&l.app)[8], "assoc:code");
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
    assert!(
        l.app
            .open_menu
            .as_ref()
            .is_some_and(|m| m.shown().len() == m.items.len()),
        "every row again"
    );
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
    // The arrows move in the field, and the bar is drawn where typing lands.
    type_text(&mut l.app, "$YYYY-$MM");
    press(&mut l.app, "<Left><Left><BS>");
    let screen = render(&mut l.app, 120, 30);
    assert!(screen.contains("$YYYY-▏MM"), "{screen}");
    press(&mut l.app, "<Right><Right><BS><BS><BS><BS><BS><BS><BS><BS>");
    let replace = l.app.rename_panel.as_ref().map(|p| p.replace.text());
    assert_eq!(replace.as_deref(), Some(""), "all of it can go");
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

/// Text files show their first lines in the preview column, whatever the encoding;
/// a binary file with an unknown extension shows its info; `zp` reads it full screen
/// and `J` scrolls.
#[test]
fn text_files_preview_their_lines() {
    let dir = std::env::temp_dir().join(format!(
        "sentier-text-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    // An old .bat in Windows-1252 (é), PowerShell 5's UTF-16 LE, no extension, binary.
    let _ = std::fs::write(dir.join("run.bat"), b"@echo off\r\necho caf\xe9\r\n");
    let mut ps = vec![0xFF, 0xFE];
    for c in "Get-ChildItem C:\\\r\nWrite-Host done\r\n".encode_utf16() {
        ps.extend(c.to_le_bytes());
    }
    let _ = std::fs::write(dir.join("deploy.ps1"), ps);
    let long: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    let _ = std::fs::write(dir.join("README"), long);
    let _ = std::fs::write(dir.join("blob.xyz"), [0u8, 1, 2, 0, 255, 0]);
    let mut l = Live::new(&dir);
    let shown = |l: &mut Live, name: &str| {
        l.goto(name);
        l.app.preview_due = Some(Instant::now());
        l.until(|a| {
            a.text_preview
                .as_ref()
                .is_some_and(|t| t.loc.name() == name && t.result.is_some())
        });
        render(&mut l.app, 160, 30)
    };
    let s = shown(&mut l, "run.bat");
    if std::env::var_os("SENTIER_SHOW_TEXT").is_some() {
        eprintln!("{s}");
    }
    assert!(s.contains("@echo off") && s.contains("echo café"), "{s}");
    assert!(s.contains("Windows-1252 · 2 lines"), "{s}");
    let s = shown(&mut l, "deploy.ps1");
    assert!(
        s.contains(r"Get-ChildItem C:\") && s.contains("UTF-16 LE"),
        "{s}"
    );
    let s = shown(&mut l, "blob.xyz");
    assert!(
        s.contains("Size") && !s.contains("UTF-8"),
        "binary: its info, {s}"
    );
    let s = shown(&mut l, "README");
    assert!(s.contains("line 1") && s.contains("zp: full screen"), "{s}");
    // Full screen, J scrolls half a page.
    press(&mut l.app, "zp");
    assert_eq!(l.app.mode, Mode::Preview);
    let before = l.app.text_preview.as_ref().map_or(0, |t| t.scroll);
    press(&mut l.app, "J");
    let after = l.app.text_preview.as_ref().map_or(0, |t| t.scroll);
    assert!(after > before, "{before} → {after}");
    let s = render(&mut l.app, 160, 30);
    assert!(s.contains("J/K ^d/^u") && s.contains("scroll"), "{s}");
    press(&mut l.app, "<Esc>");
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

    // :colorscheme: Tab completes and cycles, Enter switches, and it's saved beside
    // the order (here, not in a test of its own: they'd share ui.toml).
    use crate::ui::theme;
    press(&mut l.app, ":colo tok<Tab>");
    assert_eq!(line_now(&l.app), "colo tokyonight-night");
    press(&mut l.app, "<Tab>");
    assert_eq!(line_now(&l.app), "colo tokyonight-storm");
    press(&mut l.app, "<CR>");
    assert_eq!(theme::scheme(), "tokyonight-storm");
    assert_eq!(status(&l.app), "colorscheme tokyonight-storm");
    l.until(|_| {
        let st = crate::state::load(&saved);
        st.colorscheme == "tokyonight-storm" && st.sort == "ext"
    });
    press(&mut l.app, ":colorscheme gruvbox<CR>");
    assert!(status(&l.app).contains("unknown colorscheme"));
    assert_eq!(theme::scheme(), "tokyonight-storm", "unchanged");
    press(&mut l.app, ":colorscheme<CR>");
    assert!(status(&l.app).starts_with("colorscheme tokyonight-storm (also: default"));
    press(&mut l.app, ":colorscheme tokyonight<CR>");
    assert_eq!(theme::scheme(), "tokyonight-night", "tokyonight is night");
    press(&mut l.app, ":colorscheme default<CR>");
    assert_eq!(theme::scheme(), "default");
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
    let at =
        |a: &App, want: &str| a.cwd.display().to_lowercase() == want && a.view_listing.is_some();
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
    // "saved", or "updated" when a run with a reused process id left one behind.
    assert!(
        status(&l.app).contains(" preset upper"),
        "{}",
        status(&l.app)
    );
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
