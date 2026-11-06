//! Probe: which keys actually arrive, and how, in this terminal (CLAUDE.md "Terminal key
//! caveats"). Run it in WezTerm and in Windows Terminal:
//!
//! ```powershell
//! cargo run --example probe_keys
//! ```
//!
//! It reports whether the kitty keyboard protocol is available (and pushes it if so), then
//! prints every key event. Press, in order, and note what each line says:
//!
//! 1. `Ctrl-Space`  (launcher alias: arrives at all? as Char(' ')+CONTROL, or Null?)
//! 2. `Shift-Enter` (`:sync!` alias: distinct from Enter?)
//! 3. `Tab`, then `Ctrl-i` (same event, or distinct?)
//! 4. `Enter`, then `Ctrl-m`
//! 5. `Esc`         (instant, or delayed/merged with the next key?)
//! 6. `Alt-Enter`   (Properties; WezTerm may toggle fullscreen instead)
//! 7. `Ctrl-o`, `Ctrl-d`, `Ctrl-u`, `Shift-G`, `'`, `\`
//!
//! Press `q` twice in a row (or Ctrl-c) to quit.

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use std::io::Write;

fn main() -> std::io::Result<()> {
    let supported = supports_keyboard_enhancement();
    println!("supports_keyboard_enhancement() = {supported:?}");
    println!(
        "TERM_PROGRAM={:?}  WT_SESSION set: {}",
        std::env::var("TERM_PROGRAM").ok(),
        std::env::var_os("WT_SESSION").is_some()
    );
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    let pushed = matches!(supported, Ok(true))
        && execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )
        .is_ok();
    write!(out, "keyboard enhancement pushed: {pushed}\r\n\r\n")?;
    write!(out, "press keys; q q (or Ctrl-c) quits\r\n")?;
    out.flush()?;

    let mut last_q = false;
    let result = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => break Err(e),
        };
        let Event::Key(k) = ev else {
            write!(out, "  other: {ev:?}\r\n")?;
            continue;
        };
        write!(
            out,
            "  {:<8} code={:<22} mods={:<24} state={:?}\r\n",
            format!("{:?}", k.kind),
            format!("{:?}", k.code),
            format!("{:?}", k.modifiers),
            k.state
        )?;
        out.flush()?;
        if k.kind == KeyEventKind::Release {
            continue;
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            break Ok(());
        }
        let is_q = k.code == KeyCode::Char('q') && k.modifiers.is_empty();
        if is_q && last_q {
            break Ok(());
        }
        last_q = is_q;
    };
    if pushed {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    println!();
    result
}
