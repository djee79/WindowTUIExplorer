//! sentier: a Windows-first terminal file explorer.

mod app;
mod auth;
mod cli;
mod config;
mod event;
mod frecency;
mod fs;
mod help;
mod jobs;
mod keymap;
mod launcher;
mod locations;
mod machines;
mod open;
mod picker;
mod pin_backups;
mod pins;
mod platform;
mod preview;
mod prompt;
mod rename;
mod search;
mod state;
mod ui;

use anyhow::{Context, Result, bail};
use app::{App, Quit};
use event::AppEvent;
use fs::path::Loc;
use fs::worker::Worker;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::{DefaultTerminal, Terminal, backend::CrosstermBackend};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

const USAGE: &str = "\
sentier: a terminal file explorer for Windows

usage: sentier [options] [path]
       sentier run <preset name> [--allow-mirror]   (headless; sentier run --help)
       sentier pin <path|url> [--name N] [--tag T]... [--key K]
       sentier pins [--query \"#tag is:unc\"] [--json]
       sentier open <pin name>

  path               start here (C:\\dir, \\\\server\\share\\dir, relative, ...;
                     a folder named run, pin, pins or open: sentier .\\run)

options:
  --cwd-file <file>  on ZZ (or Q, :wq), write the current directory to <file>
  -h, --help         show this help
  -V, --version      show the version
";

struct Args {
    cwd_file: Option<PathBuf>,
    start: Option<String>,
}

fn main() -> ExitCode {
    // Electron apps (VS Code, Teams, Slack…) started from here attach to their parent's
    // console and print their logs into it: started by Windows' own handlers from inside
    // sentier, that parent is us, and the log lands on top of the TUI. Electron's switch
    // turns that off for every program we start (they inherit our environment).
    // SAFETY: first thing in main, before any thread exists, so nothing reads the
    // environment concurrently.
    unsafe {
        std::env::set_var("ELECTRON_NO_ATTACH_CONSOLE", "1");
    }
    let all: Vec<String> = std::env::args().skip(1).collect();
    match all.first().map(String::as_str) {
        Some("run") => return ExitCode::from(run_headless(&all[1..])),
        Some(cmd @ ("pin" | "pins" | "open")) => return ExitCode::from(run_cli(cmd, &all[1..])),
        _ => {}
    }
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The TUI is down by now; stderr is safe again.
            eprintln!("sentier: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `sentier pin` / `pins` / `open` (cli.rs): no TUI. 0 ok, 1 refused, 2 bad usage.
fn run_cli(cmd: &str, args: &[String]) -> u8 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", cli::USAGE);
        return 0;
    }
    let _log_guard = init_logging();
    let Some(path) = pins::store_path() else {
        eprintln!("sentier {cmd}: no %APPDATA% folder, so no pins.toml");
        return 1;
    };
    let mut store = match pins::load(&path) {
        Ok((store, _)) => store,
        Err(e) => {
            eprintln!("sentier {cmd}: {e}");
            return 1;
        }
    };
    match cmd {
        "pin" => {
            let parsed = match cli::parse_pin_args(args) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("sentier pin: {e}\n\n{}", cli::USAGE);
                    return 2;
                }
            };
            let cwd = std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            let probe = |p: &str| std::fs::metadata(p).ok().map(|m| m.is_dir());
            match cli::add_pin(&mut store, &parsed, &cwd, &probe) {
                Ok(msg) => match pins::save(&path, &store) {
                    // A running sentier sees the file change and reloads it.
                    Ok(()) => {
                        println!("{msg}");
                        0
                    }
                    Err(e) => {
                        eprintln!("sentier pin: {e}");
                        1
                    }
                },
                Err(e) => {
                    eprintln!("sentier pin: {e}");
                    1
                }
            }
        }
        "pins" => {
            let mut query = String::new();
            let mut json = false;
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => json = true,
                    "--query" | "-q" => query = it.next().cloned().unwrap_or_default(),
                    other => {
                        eprintln!("sentier pins: unknown option {other}\n\n{}", cli::USAGE);
                        return 2;
                    }
                }
            }
            let found = cli::matching(&store, &query);
            if json {
                match cli::to_json(&found) {
                    Ok(text) => println!("{text}"),
                    Err(e) => {
                        eprintln!("sentier pins: {e}");
                        return 1;
                    }
                }
            } else {
                cli::to_lines(&found, &mut std::io::stdout());
            }
            0
        }
        _ => {
            let name = args.join(" ");
            let Some(pin) = cli::find(&store, &name) else {
                eprintln!("sentier open: no pin named {name:?} (sentier pins lists them)");
                return 1;
            };
            match cli_open(pin) {
                Ok(what) => {
                    // It counts for the launcher's frecency, as in the TUI.
                    if let (Some(key), Some(up)) = (frecency::pin_key(pin), frecency::store_path())
                    {
                        let mut usage = frecency::load(&up);
                        usage.record(&key, frecency::now_secs());
                        let _ = frecency::save(&up, &usage);
                    }
                    println!("{what}");
                    0
                }
                Err(e) => {
                    eprintln!("sentier open: {e}");
                    1
                }
            }
        }
    }
}

/// Opens a pin as Enter would, without the TUI.
fn cli_open(pin: &pins::Pin) -> Result<String, String> {
    use pins::PinKind;
    match pin.kind {
        PinKind::App | PinKind::Web => {
            let l = launcher::launch_of(pin).ok_or("it has no command or url")?;
            let command = if l.find {
                open::run::resolve(&l.command)
                    .ok_or_else(|| format!("{} isn't installed", l.command))?
            } else {
                l.command
            };
            platform::shell_execute(&command, &l.args, &l.dir).map_err(|e| e.to_string())?;
            Ok(format!("started {}", pin.name))
        }
        PinKind::Folder | PinKind::File => {
            let target = pin.target().ok_or("it has no valid path")?;
            let path = target.as_fs().ok_or("it has no valid path")?.os_string();
            platform::shell_open(&path).map_err(|e| e.to_string())?;
            Ok(format!("opened {}", pin.name))
        }
        PinKind::Machine => {
            let host = pin.extra.get("host").and_then(|v| v.as_str()).unwrap_or("");
            platform::shell_open(&format!(r"\\{host}\C$")).map_err(|e| e.to_string())?;
            Ok(format!(r"opened \\{host}\C$"))
        }
        PinKind::Sync | PinKind::Backup => Err(format!(
            "{} is a preset: sentier run \"{}\" runs it",
            pin.name, pin.name
        )),
    }
}

/// `sentier run <preset>`: no TUI, no raw mode; stdout is the scheduled task's output.
fn run_headless(args: &[String]) -> u8 {
    use jobs::headless::{self, EXIT_FATAL};
    let fatal = EXIT_FATAL as u8;
    let mut args = match headless::parse_args(args) {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{}", headless::USAGE);
            return 0;
        }
        Err(e) => {
            eprintln!("sentier run: {e}\n\n{}", headless::USAGE);
            return fatal;
        }
    };
    if args.threads == 0 {
        // No --threads: config.toml's jobs.robocopy_threads, else the default (8).
        let (config, problem) = config::load_early();
        if let Some(p) = problem {
            eprintln!("note: {p}");
        }
        args.threads = config.jobs.threads();
    }
    let _log_guard = init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), ?args, "headless run");
    let Some(path) = pins::store_path() else {
        eprintln!("sentier run: no %APPDATA% folder, so no pins.toml");
        return fatal;
    };
    let store = match pins::load(&path) {
        Ok((store, notes)) => {
            for n in notes {
                eprintln!("note: {n}");
            }
            store
        }
        Err(e) => {
            eprintln!("sentier run: {e}");
            return fatal;
        }
    };
    let code = headless::run(&store, &args, &mut std::io::stdout());
    tracing::info!(code, "headless run done");
    code.clamp(0, 255) as u8
}

fn real_main() -> Result<()> {
    let Some(args) = parse_args(std::env::args().skip(1))? else {
        return Ok(());
    };
    let _log_guard = init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    let start = start_location(args.start.as_deref())?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .thread_name("sentier-worker")
        .build()
        .context("starting the async runtime")?;
    let (tx, rx) = event::channel();
    // Timeouts and the picture protocol are needed before anything is listed.
    let (early, problem) = config::load_early();
    let worker = Worker::new(rt.handle().clone(), tx.clone(), early.timeouts());

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    spawn_input_thread(tx);

    let mut app = App::new(worker, start);
    app.apply_early_config(early, problem);
    let result = app::run(&mut terminal, &mut app, &rx, &TermHooks);
    restore_terminal();

    // Don't wait for readers stuck on dead shares; the OS reclaims them at exit.
    rt.shutdown_background();

    // Folder visits since the last periodic save (the worker may be gone by now).
    if let (Some(usage), Some(path)) = (app.take_unsaved_usage(), frecency::store_path())
        && let Err(e) = frecency::save(&path, &usage)
    {
        tracing::warn!(error = %e, "saving usage.tsv at exit");
    }
    // A folder order changed in the last half second.
    if let (Some(st), Some(path)) = (app.take_unsaved_sort(), state::path())
        && let Err(e) = state::save(&path, &st)
    {
        tracing::warn!(error = %e, "saving ui.toml at exit");
    }
    let quit = result?;
    if let (Quit::WithCwd, Some(file)) = (quit, &args.cwd_file) {
        // Virtual locations (Locations root, a server node) have no shell equivalent:
        // write nothing and the shell stays where it was.
        // Search results write the folder they were searched from.
        let dir = app
            .cwd
            .real()
            .as_fs()
            .map(|p| p.display())
            .unwrap_or_default();
        std::fs::write(file, dir)
            .with_context(|| format!("writing cwd file {}", file.display()))?;
    }
    tracing::info!("bye");
    Ok(())
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Option<Args>> {
    let mut args = Args {
        cwd_file: None,
        start: None,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("sentier {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--cwd-file" => {
                let Some(f) = it.next() else {
                    bail!("--cwd-file needs a file path");
                };
                args.cwd_file = Some(PathBuf::from(f));
            }
            flag if flag.starts_with("--") => bail!("unknown option {flag}\n\n{USAGE}"),
            path => {
                if args.start.replace(path.to_string()).is_some() {
                    bail!("only one start path, please");
                }
            }
        }
    }
    Ok(Some(args))
}

/// The start path, parsed (not checked: the worker finds out if it exists).
fn start_location(arg: Option<&str>) -> Result<Loc> {
    let here = std::env::current_dir()
        .ok()
        .and_then(|d| Loc::parse(&d.to_string_lossy()).ok())
        .unwrap_or(Loc::Root);
    match arg {
        None => Ok(here),
        Some(a) => Loc::resolve(&here, a).with_context(|| format!("bad start path {a:?}")),
    }
}

/// Logs to `%LOCALAPPDATA%\sentier\logs\`, never to the terminal. Returns the guard that
/// flushes the background writer on drop, or None if logging couldn't be set up (the
/// app runs fine without it).
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("sentier").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("sentier")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_env("SENTIER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .ok()?;
    Some(guard)
}

/// Set when the kitty keyboard protocol was pushed, so teardown pops it.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn setup_terminal() -> Result<DefaultTerminal> {
    enable_raw_mode().context("enabling raw mode")?;
    if let Err(e) = execute!(std::io::stdout(), EnterAlternateScreen) {
        restore_terminal();
        return Err(e).context("entering the alternate screen");
    }
    push_keyboard_flags();
    tracing::info!(
        keyboard_enhancement = KEYBOARD_ENHANCED.load(Ordering::Relaxed),
        "terminal ready"
    );
    match Terminal::new(CrosstermBackend::new(std::io::stdout())) {
        Ok(t) => Ok(t),
        Err(e) => {
            restore_terminal();
            Err(e).context("creating the terminal")
        }
    }
}

fn push_keyboard_flags() {
    // Distinct Ctrl-Space / Shift-Enter / Ctrl-i where the terminal can say so. Every
    // binding that needs this has a plain-key equivalent (CLAUDE.md "Terminal key
    // caveats"), so not having it is fine. examples/probe_keys.rs shows what arrives.
    if matches!(supports_keyboard_enhancement(), Ok(true))
        && execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    {
        KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
    }
}

/// Set while an inline program (`:rename` in the editor) has the terminal: the input
/// thread stops reading keys (and says so with `INPUT_PARKED`).
static INPUT_PAUSED: AtomicBool = AtomicBool::new(false);
static INPUT_PARKED: AtomicBool = AtomicBool::new(false);

/// Suspends and resumes the TUI around an inline program.
struct TermHooks;

impl app::Suspend for TermHooks {
    fn suspend(&self) {
        INPUT_PAUSED.store(true, Ordering::SeqCst);
        let end = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while !INPUT_PARKED.load(Ordering::SeqCst) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        restore_terminal();
    }

    fn resume(&self) {
        let _ = enable_raw_mode();
        let _ = execute!(std::io::stdout(), EnterAlternateScreen);
        push_keyboard_flags();
        INPUT_PAUSED.store(false, Ordering::SeqCst);
    }
}

/// Leaves raw mode and the alternate screen. Safe to call more than once.
fn restore_terminal() {
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
}

/// Rule 5: a panic must leave the terminal usable before the message is printed.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        tracing::error!(%info, "panic");
        default(info);
    }));
}

/// Blocking terminal reads live on their own thread and arrive as events.
fn spawn_input_thread(tx: event::EventTx) {
    let spawned = std::thread::Builder::new()
        .name("sentier-input".to_string())
        .spawn(move || {
            loop {
                // Paused while an inline program owns the terminal: don't steal its keys.
                if INPUT_PAUSED.load(Ordering::SeqCst) {
                    INPUT_PARKED.store(true, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                INPUT_PARKED.store(false, Ordering::SeqCst);
                match ratatui::crossterm::event::poll(std::time::Duration::from_millis(50)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        let _ = tx.send(AppEvent::InputClosed(e.to_string()));
                        return;
                    }
                }
                match ratatui::crossterm::event::read() {
                    Ok(ev) => {
                        if tx.send(AppEvent::Term(ev)).is_err() {
                            return; // UI is gone
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::InputClosed(e.to_string()));
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "couldn't start the input thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Option<Args>> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn arg_parsing() {
        let a = args(&["--cwd-file", "f.txt", r"\\srv\share"])
            .ok()
            .flatten();
        let a = a.expect("parsed");
        assert_eq!(a.cwd_file, Some(PathBuf::from("f.txt")));
        assert_eq!(a.start.as_deref(), Some(r"\\srv\share"));

        assert!(args(&["--cwd-file"]).is_err());
        assert!(args(&["--nope"]).is_err());
        assert!(args(&["a", "b"]).is_err());
        assert!(matches!(args(&["--help"]), Ok(None)));
    }
}
