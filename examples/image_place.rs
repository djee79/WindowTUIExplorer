//! M2b probe: where does an iTerm2 picture land under WezTerm + ConPTY?
//!
//! In sentier the preview picture appeared at the wrong place (bottom right, cut off).
//!
//! Round 1 found: a picture inside a full ratatui frame lands right; written after the
//! frame (cursor move + picture, with or without a wait) it lands wrong. Theory: ratatui
//! hides the cursor, so ConPTY never sends cursor moves to the real terminal; the real
//! cursor sits wherever ConPTY last painted text, and the picture lands there. In a full
//! frame the border just before the picture was painted, so the cursor happened to be
//! right; in sentier only the picture changes, so nothing is painted before it.
//!
//! Round 2 tests that, with two target boxes on a busy screen:
//!   1. small update: a full frame, then a frame where only the pictures are new (sentier)
//!   2. after the frame: show the cursor, move it, write the picture, hide it; one write
//!   3. the same, waiting 15 ms after the cursor move
//!   4. full frame with the pictures inside (round 1's case, as a control)
//!
//! For each: `y` both pictures inside their boxes · `p` one of them · `n` neither · `q` quit.
//!
//! ```text
//! cargo run --example image_place
//! ```
//! The report goes to the screen and `%TEMP%\sentier_image_place.txt`.

use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::crossterm::cursor::{Hide, MoveTo, Show};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::queue;
use ratatui::crossterm::style::Print;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use std::fmt::Write as _;
use std::io::Write;
use std::time::Duration;

#[derive(Clone, Copy)]
enum Way {
    SmallUpdate,
    ShowCursor(u64),
    FullFrame,
}

impl Way {
    fn label(self) -> String {
        match self {
            Way::SmallUpdate => "1 small update, only the pictures new".to_string(),
            Way::ShowCursor(0) => "2 after: show cursor, move, picture, hide".to_string(),
            Way::ShowCursor(ms) => format!("3 after: show cursor, move, wait {ms} ms"),
            Way::FullFrame => "4 full frame with the pictures (control)".to_string(),
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
    let _ = writeln!(s, "sentier image placement probe, round 2 (iTerm2)");
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

/// The busy screen with the two boxes, and the pictures when given.
fn draw(f: &mut Frame, n: usize, way: Way, boxes: [Rect; 2], pics: Option<[&Protocol; 2]>) {
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
    for r in boxes {
        f.render_widget(Clear, r);
        f.render_widget(
            Block::bordered()
                .border_type(BorderType::Double)
                .border_style(Style::new().fg(Color::Cyan)),
            r,
        );
    }
    if let Some(pics) = pics {
        for (r, p) in boxes.iter().zip(pics) {
            f.render_widget(Image::new(p), Block::bordered().inner(*r));
        }
    }
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
    let boxes = [a, b];
    let inner = [Block::bordered().inner(a), Block::bordered().inner(b)];
    let mut pics = Vec::new();
    for r in inner {
        pics.push(picker.new_protocol(
            card.clone(),
            Size::new(r.width, r.height),
            Resize::Fit(None),
        )?);
    }
    let both = [&pics[0], &pics[1]];

    let mut answers = Vec::new();
    let ways = [
        Way::SmallUpdate,
        Way::ShowCursor(0),
        Way::ShowCursor(15),
        Way::FullFrame,
    ];
    for (n, way) in ways.into_iter().enumerate() {
        terminal.clear()?;
        match way {
            Way::SmallUpdate => {
                terminal.draw(|f| draw(f, n, way, boxes, None))?;
                std::thread::sleep(Duration::from_millis(300));
                terminal.draw(|f| draw(f, n, way, boxes, Some(both)))?;
            }
            Way::ShowCursor(ms) => {
                terminal.draw(|f| draw(f, n, way, boxes, None))?;
                std::thread::sleep(Duration::from_millis(300));
                for (p, r) in both.iter().zip(inner) {
                    place(p, r, ms)?;
                }
            }
            Way::FullFrame => {
                terminal.draw(|f| draw(f, n, way, boxes, Some(both)))?;
            }
        }
        let answer = ask()?;
        // Remove both pictures (spaces over their cells) before the next round.
        for r in inner {
            erase(r)?;
        }
        match answer {
            Some(a) => answers.push((way.label(), a.to_string())),
            None => break,
        }
    }
    terminal.clear()?;
    Ok(answers)
}

/// After the frame: show the cursor so ConPTY really moves it, move it to `at`, write the
/// picture, hide the cursor again.
fn place(p: &Protocol, at: Rect, wait_ms: u64) -> std::io::Result<()> {
    let Protocol::ITerm2(i) = p else {
        return Ok(());
    };
    let mut out = std::io::stdout();
    queue!(out, Show, MoveTo(at.x, at.y))?;
    if wait_ms > 0 {
        out.flush()?;
        std::thread::sleep(Duration::from_millis(wait_ms));
    }
    queue!(out, Print(&i.data), Hide)?;
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
