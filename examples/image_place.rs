//! M2b probe: where does an iTerm2 picture land under WezTerm + ConPTY?
//!
//! In sentier the preview picture appeared at the wrong place (bottom right, cut off).
//! Suspected cause: ConPTY interprets the cursor moves around the picture and repaints on
//! its own schedule, but passes the picture itself straight through, so the picture lands
//! wherever the real terminal's cursor happens to be.
//!
//! This draws a busy screen (like sentier's columns) with two target boxes, and places the
//! test card in both, four ways:
//!   1. inline: inside the ratatui frame (what sentier does now)
//!   2. after: after the frame, move the cursor there and write the picture, one write
//!   3. after + 30 ms: move the cursor, flush, wait 30 ms, then write the picture
//!   4. after + 100 ms: the same with 100 ms
//!
//! For each: `y` both pictures inside their boxes · `p` one of them · `n` neither · `q` quit.
//!
//! ```text
//! cargo run --example image_place
//! ```
//! The report goes to the screen and `%TEMP%\sentier_image_place.txt`.

use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::queue;
use ratatui::crossterm::style::Print;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use std::fmt::Write as _;
use std::io::Write;
use std::time::Duration;

#[derive(Clone, Copy)]
enum Way {
    Inline,
    After(u64),
}

impl Way {
    fn label(self) -> String {
        match self {
            Way::Inline => "1 inline (in the ratatui frame)".to_string(),
            Way::After(0) => "2 after the frame, one write".to_string(),
            Way::After(ms) => format!("after the frame, cursor first, wait {ms} ms"),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let card = test_card();
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &card);
    ratatui::restore();
    let answers = result?;

    let mut s = String::new();
    let _ = writeln!(s, "sentier image placement probe (iTerm2)");
    let _ = writeln!(
        s,
        "  TERM_PROGRAM {}  {}",
        std::env::var("TERM_PROGRAM").unwrap_or_default(),
        std::env::var("TERM_PROGRAM_VERSION").unwrap_or_default()
    );
    for (way, a) in &answers {
        let _ = writeln!(s, "  {way:<46} {a}");
    }
    println!("{s}");
    let path = std::env::temp_dir().join("sentier_image_place.txt");
    if std::fs::write(&path, &s).is_ok() {
        println!("Saved to {}", path.display());
    }
    Ok(())
}

fn run(
    terminal: &mut DefaultTerminal,
    card: &DynamicImage,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Iterm2);
    let size = terminal.size()?;
    // Like sentier: a box on the right near the top, and one bottom left.
    let a = Rect::new(
        size.width * 55 / 100,
        3,
        size.width * 40 / 100,
        size.height / 2,
    );
    let b = Rect::new(4, size.height.saturating_sub(16), size.width / 4, 13);
    let mut answers = Vec::new();
    let ways = [Way::Inline, Way::After(0), Way::After(30), Way::After(100)];
    for (n, way) in ways.into_iter().enumerate() {
        let inner_a = Block::bordered().inner(a);
        let inner_b = Block::bordered().inner(b);
        let pa = picker.new_protocol(
            card.clone(),
            Size::new(inner_a.width, inner_a.height),
            Resize::Fit(None),
        )?;
        let pb = picker.new_protocol(
            card.clone(),
            Size::new(inner_b.width, inner_b.height),
            Resize::Fit(None),
        )?;

        // A busy frame first, so ConPTY has painting to do.
        terminal.draw(|f| {
            let area = f.area();
            let lines: Vec<Line> = (0..area.height)
                .map(|i| {
                    Line::from(format!(
                        " {i:>3}  {}",
                        "lorem ipsum dolor sit amet consectetur ".repeat(6)
                    ))
                })
                .collect();
            f.render_widget(
                Paragraph::new(lines).style(Style::new().fg(Color::DarkGray)),
                area,
            );
            let title = format!(
                " {}/4  {}  ·  pictures inside both boxes?  y both · p one · n neither · q quit ",
                n + 1,
                way.label()
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    title,
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ))),
                Rect::new(0, 0, area.width, 1),
            );
            for r in [a, b] {
                f.render_widget(ratatui::widgets::Clear, r);
                f.render_widget(
                    Block::bordered()
                        .border_type(BorderType::Double)
                        .border_style(Style::new().fg(Color::Cyan)),
                    r,
                );
            }
            if let Way::Inline = way {
                f.render_widget(Image::new(&pa), inner_a);
                f.render_widget(Image::new(&pb), inner_b);
            }
        })?;
        if let Way::After(ms) = way {
            place(&pa, inner_a, ms)?;
            place(&pb, inner_b, ms)?;
        }
        let answer = ask()?;
        // Remove both pictures (spaces over their cells) before the next round.
        erase(inner_a)?;
        erase(inner_b)?;
        terminal.clear()?;
        match answer {
            Some(a) => answers.push((way.label(), a.to_string())),
            None => break,
        }
    }
    Ok(answers)
}

/// After the frame: move the cursor to `at`, optionally wait for ConPTY to paint that,
/// then write the picture.
fn place(p: &Protocol, at: Rect, wait_ms: u64) -> std::io::Result<()> {
    let Protocol::ITerm2(i) = p else {
        return Ok(());
    };
    let mut out = std::io::stdout();
    queue!(out, MoveTo(at.x, at.y))?;
    if wait_ms > 0 {
        out.flush()?;
        std::thread::sleep(Duration::from_millis(wait_ms));
    }
    queue!(out, Print(&i.data))?;
    out.flush()
}

fn erase(rect: Rect) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    let blank = " ".repeat(rect.width as usize);
    for y in rect.y..rect.y + rect.height {
        queue!(out, MoveTo(rect.x, y), Print(&blank))?;
    }
    out.flush()
}

fn ask() -> anyhow::Result<Option<&'static str>> {
    loop {
        if !event::poll(Duration::from_secs(600))? {
            return Ok(None);
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('y') => return Ok(Some("both in place")),
            KeyCode::Char('p') => return Ok(Some("one in place")),
            KeyCode::Char('n') => return Ok(Some("neither")),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

/// Colour bars over a grey gradient, red border.
fn test_card() -> DynamicImage {
    const W: u32 = 320;
    const H: u32 = 200;
    let bars: [[u8; 3]; 6] = [
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
    ];
    DynamicImage::ImageRgba8(RgbaImage::from_fn(W, H, |x, y| {
        if x < 4 || y < 4 || x >= W - 4 || y >= H - 4 {
            Rgba([220, 40, 40, 255])
        } else if y < H / 2 {
            let [r, g, b] = bars[(x * 6 / W) as usize];
            Rgba([r, g, b, 255])
        } else {
            let v = (x * 255 / W) as u8;
            Rgba([v, v, v, 255])
        }
    }))
}
