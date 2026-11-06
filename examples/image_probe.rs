//! M2b probe (CLAUDE.md section 11, "Verify first"): which terminal image protocols really
//! display under WezTerm and Windows Terminal on Windows, with ConPTY in between.
//!
//! Run it in the terminal you want to test (not inside tmux, nvim or another TUI):
//!
//! ```text
//! cargo run --example image_probe              # built-in test card
//! cargo run --example image_probe -- some.jpg  # or your own picture
//! ```
//!
//! For each protocol (halfblocks, sixel, kitty, iTerm2) it draws the picture and asks:
//! `y` shows correctly · `p` partly (garbled, wrong size, flickers) · `n` nothing / junk.
//! Then the picture moves and shrinks, and it asks whether the old copy is gone (the spec's
//! "never leave the previous file's image on screen"). `q` stops early.
//!
//! At the end it prints a report and writes it to `%TEMP%\sentier_image_probe.txt`.
//! The report holds terminal names and versions only, no paths or user names.
//!
//! `cargo run --example image_probe -- --ghost` (iTerm2 only) instead tests ways to remove
//! an old picture from the screen; see the `--ghost` section at the end.

use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use ratatui_image::StatefulImage;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

const PROTOCOLS: [ProtocolType; 4] = [
    ProtocolType::Halfblocks,
    ProtocolType::Sixel,
    ProtocolType::Kitty,
    ProtocolType::Iterm2,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Yes,
    Partly,
    No,
    Skipped,
}

impl Answer {
    fn label(self) -> &'static str {
        match self {
            Answer::Yes => "yes",
            Answer::Partly => "partly",
            Answer::No => "no",
            Answer::Skipped => "-",
        }
    }
}

struct Outcome {
    protocol: ProtocolType,
    shows: Answer,
    old_gone: Answer,
    encode_error: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let ghost = args.first().is_some_and(|a| a == "--ghost");
    if ghost {
        args.remove(0);
    }
    let image = match args.first() {
        Some(path) => image::open(path)?,
        None => test_card(),
    };
    let env = terminal_env();
    if ghost {
        return ghost_main(&env, &image);
    }

    // ratatui::init enters the alternate screen + raw mode and installs a panic hook that
    // restores the terminal.
    let mut terminal = ratatui::init();
    // The query must run after entering the alternate screen, before reading events.
    let started = Instant::now();
    let query = Picker::from_query_stdio();
    let query_ms = started.elapsed().as_millis();
    let (mut picker, detected) = match query {
        Ok(p) => {
            let d = format!(
                "{:?}, cell {}x{} px",
                p.protocol_type(),
                p.font_size().width,
                p.font_size().height
            );
            (p, d)
        }
        Err(e) => (Picker::halfblocks(), format!("query failed: {e}")),
    };

    let result = run(&mut terminal, &mut picker, &image);
    ratatui::restore();
    let outcomes = result?;

    let report = report(&env, &detected, query_ms, &outcomes);
    println!("{report}");
    let path = std::env::temp_dir().join("sentier_image_probe.txt");
    match std::fs::write(&path, &report) {
        Ok(()) => println!("Saved to {}", path.display()),
        Err(e) => println!("(couldn't save the report: {e})"),
    }
    Ok(())
}

fn run(
    terminal: &mut DefaultTerminal,
    picker: &mut Picker,
    image: &DynamicImage,
) -> anyhow::Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for (i, protocol) in PROTOCOLS.into_iter().enumerate() {
        picker.set_protocol_type(protocol);
        let mut state = picker.new_resize_protocol(image.clone());
        let title = format!("{}/{}  {protocol:?}", i + 1, PROTOCOLS.len());

        // Step 1: does it show?
        terminal.clear()?;
        terminal.draw(|f| {
            draw_step(
                f,
                &title,
                "Do you see the test card (colour bars, a white diagonal, red border)?",
                "y yes, correctly   p partly / garbled / wrong size   n nothing or junk text   q quit",
                &mut state,
                false,
            )
        })?;
        let encode_error = state
            .last_encoding_result()
            .and_then(|r| r.err())
            .map(|e| e.to_string());
        let Some(shows) = ask()? else {
            break;
        };

        // Step 2: move + shrink it; is the old copy gone?
        let old_gone = if shows == Answer::No {
            Answer::Skipped
        } else {
            terminal.draw(|f| {
                draw_step(
                    f,
                    &title,
                    "The picture moved right and got smaller. Is the OLD, bigger copy completely gone?",
                    "y gone   p traces left   n old copy still there   q quit",
                    &mut state,
                    true,
                )
            })?;
            match ask()? {
                Some(a) => a,
                None => {
                    outcomes.push(Outcome {
                        protocol,
                        shows,
                        old_gone: Answer::Skipped,
                        encode_error,
                    });
                    break;
                }
            }
        };
        outcomes.push(Outcome {
            protocol,
            shows,
            old_gone,
            encode_error,
        });
    }
    // Leave nothing behind (kitty images live outside the cell grid).
    terminal.clear()?;
    Ok(outcomes)
}

fn draw_step(
    f: &mut Frame,
    title: &str,
    question: &str,
    keys: &str,
    state: &mut StatefulProtocol,
    moved: bool,
) {
    let area = f.area();
    f.render_widget(Clear, area);
    let [head, body] = Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(" sentier image probe · {title}"),
                bold.fg(Color::Yellow),
            )),
            Line::from(format!(" {question}")),
            Line::from(Span::styled(
                format!(" {keys}"),
                Style::new().fg(Color::Gray),
            )),
        ]),
        head,
    );
    // Step 1: a big box on the left. Step 2: a smaller one further right.
    let rect = if moved {
        Rect {
            x: body.x + body.width / 2,
            y: body.y + 2,
            width: (body.width / 3).max(10),
            height: (body.height / 3).max(5),
        }
    } else {
        Rect {
            x: body.x + 2,
            y: body.y,
            width: (body.width * 2 / 3).max(20),
            height: body.height.saturating_sub(1).max(8),
        }
    }
    .intersection(body);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_stateful_widget(StatefulImage::default(), inner, state);
}

/// Waits for y / p / n (None on q or Esc).
fn ask() -> anyhow::Result<Option<Answer>> {
    loop {
        if !event::poll(Duration::from_secs(600))? {
            return Ok(None);
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // Windows also sends key releases.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('y') => return Ok(Some(Answer::Yes)),
            KeyCode::Char('p') => return Ok(Some(Answer::Partly)),
            KeyCode::Char('n') => return Ok(Some(Answer::No)),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

/// 320×200: eight colour bars on top, a grey gradient below, a white diagonal and a red
/// border. Easy to judge for sharpness, colour and cropping.
fn test_card() -> DynamicImage {
    const W: u32 = 320;
    const H: u32 = 200;
    let bars: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    let img = RgbaImage::from_fn(W, H, |x, y| {
        if x < 4 || y < 4 || x >= W - 4 || y >= H - 4 {
            return Rgba([220, 40, 40, 255]);
        }
        // The diagonal, 3 px thick.
        if (x as i64 - (y as i64 * W as i64 / H as i64)).abs() <= 2 {
            return Rgba([255, 255, 255, 255]);
        }
        if y < H / 2 {
            let [r, g, b] = bars[(x * 8 / W) as usize];
            Rgba([r, g, b, 255])
        } else {
            let v = (x * 255 / W) as u8;
            Rgba([v, v, v, 255])
        }
    });
    DynamicImage::ImageRgba8(img)
}

/// Terminal identification from the environment. Names and versions only.
fn terminal_env() -> Vec<(&'static str, String)> {
    let get = |k: &str| std::env::var(k).unwrap_or_default();
    let set = |k: &str| {
        if std::env::var_os(k).is_some() {
            "set".to_string()
        } else {
            String::new()
        }
    };
    vec![
        ("TERM_PROGRAM", get("TERM_PROGRAM")),
        ("TERM_PROGRAM_VERSION", get("TERM_PROGRAM_VERSION")),
        ("TERM", get("TERM")),
        ("COLORTERM", get("COLORTERM")),
        ("WEZTERM_EXECUTABLE", set("WEZTERM_EXECUTABLE")),
        ("WT_SESSION (Windows Terminal)", set("WT_SESSION")),
    ]
}

fn report(env: &[(&str, String)], detected: &str, query_ms: u128, outcomes: &[Outcome]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "sentier image probe");
    for (k, v) in env {
        let v = if v.is_empty() { "(unset)" } else { v.as_str() };
        let _ = writeln!(s, "  {k:<30} {v}");
    }
    let _ = writeln!(s, "  {:<30} {detected}", "auto-detected");
    let _ = writeln!(s, "  {:<30} {query_ms} ms", "terminal query took");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  {:<12} {:<8} {:<10} encode error",
        "protocol", "shows", "old gone"
    );
    for o in outcomes {
        let _ = writeln!(
            s,
            "  {:<12} {:<8} {:<10} {}",
            format!("{:?}", o.protocol),
            o.shows.label(),
            o.old_gone.label(),
            o.encode_error.as_deref().unwrap_or("-")
        );
    }
    s
}

// ------------------------------------------------------------------------------ --ghost
//
// `cargo run --example image_probe -- --ghost`: iTerm2 only. Its pictures stay on screen
// after the cells under them are drawn blank again, so this tries the ways sentier could
// remove an old preview picture:
//   1. no remedy, a text preview drawn over the same place
//   2. (A) write spaces over the old picture's cells, then draw
//   3. (B) clear the whole screen, then redraw (may blink)

/// The picture's box: big on the left, or small and further right.
fn image_rect(area: Rect, moved: bool) -> Rect {
    let [_, body] = Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);
    let rect = if moved {
        Rect {
            x: body.x + body.width / 2,
            y: body.y + 2,
            width: (body.width / 3).max(10),
            height: (body.height / 3).max(5),
        }
    } else {
        Rect {
            x: body.x + 2,
            y: body.y,
            width: (body.width * 2 / 3).max(20),
            height: body.height.saturating_sub(1).max(8),
        }
    };
    rect.intersection(body)
}

/// Remedy A: write spaces straight to the terminal over `rect`, outside ratatui's diff.
fn erase(rect: Rect) -> std::io::Result<()> {
    use ratatui::crossterm::{cursor::MoveTo, queue, style::Print};
    use std::io::Write;
    let mut out = std::io::stdout();
    let blank = " ".repeat(rect.width as usize);
    for y in rect.y..rect.y + rect.height {
        queue!(out, MoveTo(rect.x, y), Print(&blank))?;
    }
    out.flush()
}

/// A frame of plain text where the picture was, like a text-file preview.
fn draw_text_over(f: &mut Frame, title: &str, question: &str, keys: &str) {
    let area = f.area();
    f.render_widget(Clear, area);
    let [head, body] = Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(" sentier image probe · {title}"),
                bold.fg(Color::Yellow),
            )),
            Line::from(format!(" {question}")),
            Line::from(Span::styled(
                format!(" {keys}"),
                Style::new().fg(Color::Gray),
            )),
        ]),
        head,
    );
    // Short lines with gaps, like source code: most cells stay blank.
    let lines: Vec<Line> = (0..body.height)
        .map(|i| Line::from(format!("  {i:>3}  fn line_{i}() {{ }}")))
        .collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn ghost_main(env: &[(&str, String)], image: &DynamicImage) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = ghost_run(&mut terminal, image);
    ratatui::restore();
    let answers = result?;

    let mut s = String::new();
    let _ = writeln!(s, "sentier image probe --ghost (iTerm2)");
    for (k, v) in env {
        let v = if v.is_empty() { "(unset)" } else { v.as_str() };
        let _ = writeln!(s, "  {k:<30} {v}");
    }
    let _ = writeln!(s);
    for (what, a) in &answers {
        let _ = writeln!(s, "  {what:<52} {}", a.label());
    }
    println!("{s}");
    let path = std::env::temp_dir().join("sentier_image_probe_ghost.txt");
    match std::fs::write(&path, &s) {
        Ok(()) => println!("Saved to {}", path.display()),
        Err(e) => println!("(couldn't save the report: {e})"),
    }
    Ok(())
}

fn ghost_run(
    terminal: &mut DefaultTerminal,
    image: &DynamicImage,
) -> anyhow::Result<Vec<(&'static str, Answer)>> {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Iterm2);
    let mut state = picker.new_resize_protocol(image.clone());
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    let big = image_rect(area, false);
    let gone = "y gone   p traces left   n still there   q quit";
    let mut answers = Vec::new();
    let step = |terminal: &mut DefaultTerminal,
                answers: &mut Vec<(&'static str, Answer)>,
                label: &'static str|
     -> anyhow::Result<bool> {
        match ask()? {
            Some(a) => {
                answers.push((label, a));
                Ok(true)
            }
            None => {
                terminal.clear()?;
                Ok(false)
            }
        }
    };

    // 0. Show it.
    terminal.clear()?;
    terminal.draw(|f| {
        draw_step(
            f,
            "1/4 iTerm2",
            "Do you see the test card?",
            "y yes   n no   q quit",
            &mut state,
            false,
        )
    })?;
    if !step(terminal, &mut answers, "picture shows")? || answers[0].1 == Answer::No {
        return Ok(answers);
    }

    // 1. No remedy: a text preview takes the place.
    terminal.draw(|f| {
        draw_text_over(
            f,
            "2/4 no remedy, text drawn over it",
            "Is the picture COMPLETELY gone, only text left?",
            gone,
        )
    })?;
    if !step(
        terminal,
        &mut answers,
        "no remedy: text over it, picture gone",
    )? {
        return Ok(answers);
    }

    // 2. Remedy A: spaces over the old cells, then the small picture.
    terminal.clear()?;
    terminal.draw(|f| draw_step(f, "3/4 setup", "(one moment)", "", &mut state, false))?;
    erase(big)?;
    terminal.draw(|f| {
        draw_step(
            f,
            "3/4 remedy A: spaces written over the old picture",
            "The picture moved right and got smaller. Is the OLD, bigger copy gone?",
            gone,
            &mut state,
            true,
        )
    })?;
    if !step(
        terminal,
        &mut answers,
        "A: spaces over old cells, old copy gone",
    )? {
        return Ok(answers);
    }

    // 3. Remedy B: full clear, then the big picture again.
    terminal.clear()?;
    terminal.draw(|f| {
        draw_step(
            f,
            "4/4 remedy B: whole screen cleared",
            "The picture is big again. Is the small copy gone?",
            gone,
            &mut state,
            false,
        )
    })?;
    if !step(terminal, &mut answers, "B: full clear, old copy gone")? {
        return Ok(answers);
    }
    // Blink check: watch three clear + redraw rounds, then answer.
    terminal.draw(|f| {
        draw_step(
            f,
            "4/4 remedy B: blink check",
            "Watch the screen: it gets cleared and redrawn 3 times in a moment…",
            "",
            &mut state,
            false,
        )
    })?;
    std::thread::sleep(Duration::from_millis(1200));
    for _ in 0..3 {
        terminal.clear()?;
        terminal.draw(|f| {
            draw_step(
                f,
                "4/4 remedy B: blink check",
                "Watch the screen: it gets cleared and redrawn 3 times in a moment…",
                "",
                &mut state,
                false,
            )
        })?;
        std::thread::sleep(Duration::from_millis(400));
    }
    terminal.draw(|f| {
        draw_step(
            f,
            "4/4 remedy B: blink check",
            "Did the screen visibly BLINK during those 3 redraws?",
            "y blinked   p barely   n no",
            &mut state,
            false,
        )
    })?;
    step(terminal, &mut answers, "B: full clear blinks")?;
    terminal.clear()?;
    Ok(answers)
}
