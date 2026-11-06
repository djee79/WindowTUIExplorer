//! Drawing. Reads `App` state only; never does I/O.

mod columns;
mod grep;
mod help;
mod locations;
mod picker;
mod statusbar;
mod theme;
mod whichkey;

use crate::app::{App, Mode};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Block;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App) {
    // Layout per docs/screens/01-main.txt: top bar, panels, status line, key hints.
    let [header, body, status, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(
        Block::new().style(Style::new().bg(theme::BG).fg(theme::FG)),
        f.area(),
    );
    statusbar::draw_header(f, header, app);
    if app.at_root() {
        locations::draw(f, body, app);
    } else {
        columns::draw(f, body, app);
    }
    statusbar::draw_status(f, status, app);
    statusbar::draw_hints(f, hints, app);
    if app.mode == Mode::Picker {
        picker::draw(f, body, app);
    }
    if app.mode == Mode::Help {
        help::draw(f, body, app);
    }
    if app.which_key_visible() {
        whichkey::draw(f, body, app);
    }
}

pub(crate) fn spinner(app: &App) -> &'static str {
    SPINNER[app.spinner_frame(std::time::Instant::now()) % SPINNER.len()]
}

/// Display width in terminal cells.
pub(crate) fn width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Cuts `s` to at most `max` cells, ending in `…` when cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let w = width(c.encode_utf8(&mut buf));
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Cuts from the left, keeping the tail: `…\deep\path`.
pub(crate) fn truncate_left(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let rev: String = s.chars().rev().collect();
    let cut = truncate(&rev, max);
    // `cut` ends in `…`; reverse back so it leads.
    cut.chars().rev().collect()
}

/// `12.3 MB`, Explorer style (1024-based, KB/MB/GB labels).
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// `12 345 678`
pub(crate) fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        let cases: &[(u64, &str)] = &[
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KB"),
            (1536, "1.5 KB"),
            (150 * 1024 * 1024, "150.0 MB"),
            (1_932_441_600, "1.8 GB"),
            (3_800_000_000, "3.5 GB"),
        ];
        for (n, want) in cases {
            assert_eq!(human_size(*n), *want);
        }
        assert_eq!(thousands(1234567), "1 234 567");
        assert_eq!(thousands(999), "999");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate_left(r"C:\a\b\c\deep", 7), r"…c\deep");
        assert_eq!(truncate("日本語テキスト", 5), "日本…");
    }
}
