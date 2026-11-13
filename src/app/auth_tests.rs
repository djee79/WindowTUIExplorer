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
