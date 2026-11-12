//! Drawing. Reads `App` state only; never does I/O.

mod columns;
mod credentials;
mod dialog;
mod grep;
mod help;
mod jobs;
mod locations;
mod netlist;
mod openwith;
mod picker;
mod rename;
mod statusbar;
mod sync;
pub mod theme;
mod whichkey;

use crate::app::{App, Mode};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
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
        Block::new().style(Style::new().bg(theme::pal().bg).fg(theme::pal().fg)),
        f.area(),
    );
    statusbar::draw_header(f, header, app);
    // The preview keeps its picture when the popup that is up can stay clear of it.
    app.body = Some(body);
    if app.mode == Mode::Preview {
        columns::draw_fullscreen(f, body, app);
    } else if app.at_root() {
        locations::draw(f, body, app);
    } else {
        columns::draw(f, body, app);
    }
    statusbar::draw_status(f, status, app);
    statusbar::draw_hints(f, hints, app);
    if app.mode == Mode::Picker {
        picker::draw(f, body, app);
    }
    if app.mode == Mode::Jobs {
        jobs::draw(f, body, app);
    }
    if app.mode == Mode::Rename || (app.mode == Mode::Prompt && app.rename_panel.is_some()) {
        rename::draw(f, body, app);
    }
    if app.mode == Mode::OpenWith {
        openwith::draw(f, body, app);
    }
    if app.mode == Mode::SyncPreview {
        sync::draw(f, body, app);
    }
    if app.mode == Mode::Help {
        help::draw(f, body, app);
    }
    if app.mode == Mode::NetList {
        netlist::draw(f, body, app);
    }
    if app.mode == Mode::Credentials {
        credentials::draw(f, body, app);
    }
    if app.mode == Mode::Prompt
        && let Some(d) = app.dialog_view()
    {
        dialog::draw(f, body, &d, app.image_drawn);
    }
    if app.which_key_visible() {
        whichkey::draw(f, body, app);
    }
}

/// A popup's box `area` in `body`, moved clear of the previewed `picture` when it would
/// cover it and fits to its left (centered in the columns left of the preview). A
/// covered picture is taken off the screen and sent again afterwards, which is slow.
pub(crate) fn clear_of(body: Rect, area: Rect, picture: Option<Rect>) -> Rect {
    let Some(picture) = picture.filter(|p| p.intersects(area)) else {
        return area;
    };
    // Up to the preview panel's border (the picture sits a margin inside it).
    let left = picture.x.saturating_sub(body.x + 2);
    if left < area.width {
        return area;
    }
    Rect {
        x: body.x + (left - area.width) / 2,
        ..area
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
    fn popups_move_clear_of_a_picture_when_they_fit_beside_it() {
        let body = Rect::new(0, 1, 200, 50);
        let menu = Rect::new(66, 12, 68, 14);
        assert_eq!(clear_of(body, menu, None), menu);
        // Beside the menu, or above it: it stays centered.
        assert_eq!(clear_of(body, menu, Some(Rect::new(140, 2, 50, 40))), menu);
        assert_eq!(clear_of(body, menu, Some(Rect::new(118, 2, 50, 8))), menu);
        // Under it: centered in the 116 columns left of the preview.
        let picture = Rect::new(118, 2, 70, 40);
        let moved = clear_of(body, menu, Some(picture));
        assert_eq!(moved, Rect::new(24, 12, 68, 14));
        assert!(!moved.intersects(picture));
        // No room there: where it was (the picture is taken down instead).
        let near = Rect::new(60, 2, 120, 40);
        assert_eq!(clear_of(body, menu, Some(near)), menu);
    }

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
