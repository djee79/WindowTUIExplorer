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
    press(&mut app, "<Esc>");
    render(&mut app, 120, 30);
    assert!(app.image_drawn.is_some(), "back after the overlay");

    // The jobs panel, the bulk rename panel and its confirm prompt cover it too.
    press(&mut app, "J");
    render(&mut app, 120, 30);
    assert!(app.image_drawn.is_none(), "jobs panel");
    press(&mut app, "J");
    press(&mut app, ":rename<CR>");
    assert!(app.rename_panel.is_some());
    render(&mut app, 120, 30);
    assert!(app.image_drawn.is_none(), "rename panel");
    app.mode = Mode::Prompt; // "Rename 13 items?" over the panel
    render(&mut app, 120, 30);
    assert!(app.image_drawn.is_none(), "rename confirm");
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

    // The which-key popup keeps to the left of the picture, which stays as it is
    // (taking it away and sending it again is what made a prefix key feel slow).
    let before = app.image_drawn;
    press(&mut app, "g");
    app.tick(Instant::now() + app.keymap.timeout + Duration::from_millis(1));
    let screen = render(&mut app, 120, 30);
    assert!(app.which_key_visible() && screen.contains("Esc cancel"));
    assert!(before.is_some());
    assert_eq!(app.image_drawn, before, "the picture stays");
    assert!(app.image_place.is_some());

    // So does the `O` menu, moved to the left of the preview.
    press(&mut app, "<Esc>O");
    let screen = render(&mut app, 120, 30);
    assert!(app.mode == Mode::OpenWith && screen.contains("type to filter"));
    assert_eq!(app.image_drawn, before, "the picture stays");

    // And a yes/no dialog. In a window too narrow for it to fit beside the
    // picture, the picture is taken down instead: drawn over, it would be left
    // with a hole once the dialog closed.
    press(&mut app, "<Esc>dd");
    render(&mut app, 120, 30);
    assert!(app.dialog_view().is_some());
    assert_eq!(app.image_drawn, before, "the picture stays");
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
    let Ok(p) = crate::preview::image::pdf(&pdf, 0, &app.graphics, key.cols, key.rows, key.view)
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
    let Ok(p) = crate::preview::image::decode(&png, &app.graphics, key.cols, key.rows, key.view)
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
