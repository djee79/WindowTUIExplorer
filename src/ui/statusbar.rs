//! Top bar (badge + path) and the two-line footer (status, key hints),
//! per docs/screens/01-main.txt. The jobs line joins the footer in M4.

use super::theme::{self, ACCENT, BAR_BG, ERR, FG, NET, OK, ON_ACCENT};
use super::{human_size, spinner, thousands, truncate_left, width};
use crate::app::{App, Mode};
use crate::fs::path::{FsRoot, Loc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Keys shown in the hint line. Only what exists; it grows with each milestone.
const HINTS: &[(&str, &str)] = &[
    ("hjkl", "move"),
    ("/", "filter"),
    (".", "hidden"),
    ("~", "home"),
    ("gl", "locations"),
    ("R", "retry"),
    ("Q", "quit + cd"),
    ("?", "help"),
];

fn bar(f: &mut Frame, area: Rect, left: Vec<Span<'static>>, right: Vec<Span<'static>>) {
    let lw: usize = left.iter().map(|s| s.width()).sum();
    let rw: usize = right.iter().map(|s| s.width()).sum();
    let mut spans = left;
    spans.push(Span::raw(
        " ".repeat((area.width as usize).saturating_sub(lw + rw)),
    ));
    spans.extend(right);
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(BAR_BG).fg(FG)),
        area,
    );
}

pub fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut right: Vec<Span> = Vec::new();
    if app.is_busy() {
        right.push(Span::styled(
            format!("{}  ", spinner(app)),
            theme::fg(ACCENT),
        ));
    }
    if let Some(server) = server_of(&app.cwd) {
        // Reachability of the cwd, as far as its last listing tells.
        let (dot, color) = match app.dir(&app.cwd) {
            Some(s) if s.error.is_some() => ("×", ERR),
            Some(s) if s.listing.is_some() => ("●", OK),
            _ => ("○", theme::DIM),
        };
        right.push(Span::styled(dot, theme::fg(color)));
        right.push(Span::styled(format!("  {server}  "), theme::fg(NET)));
    }
    let badge = Span::styled(
        " sentier ",
        Style::new()
            .bg(ACCENT)
            .fg(ON_ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    let rw: usize = right.iter().map(|s| s.width()).sum();
    let path = truncate_left(
        &app.cwd.display(),
        (area.width as usize).saturating_sub(rw + 12),
    );
    let path_color = if app.cwd.is_network() { NET } else { FG };
    let left = vec![
        badge,
        Span::raw("  "),
        Span::styled(path, theme::bold(path_color)),
    ];
    bar(f, area, left, right);
}

fn server_of(loc: &Loc) -> Option<String> {
    match loc {
        Loc::Server(s) => Some(s.clone()),
        Loc::Fs(p) => match p.root() {
            FsRoot::Share { server, .. } => Some(server.clone()),
            FsRoot::Drive(_) => None,
        },
        Loc::Root => None,
    }
}

pub fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let sep = || Span::styled("  ·  ", theme::dim());
    let mut left: Vec<Span> = vec![Span::raw(" ")];
    if app.mode == Mode::Filter {
        left.push(Span::styled(format!("/{}", app.filter), theme::fg(ACCENT)));
        left.push(Span::styled("▏", theme::fg(ACCENT)));
        left.push(sep());
        left.push(Span::styled(
            format!("{} matches   Enter keep   Esc clear", app.view.len()),
            theme::dim(),
        ));
    } else if let Some(status) = &app.status {
        let color = if status.is_error { ERR } else { FG };
        left.push(Span::styled(status.text.clone(), theme::fg(color)));
    } else {
        let total = app.view_listing.as_ref().map_or(0, |l| l.entries.len());
        let shown = app.view.len();
        left.push(Span::raw(format!("{} items", thousands(total as u64))));
        if shown != total {
            left.push(Span::styled(
                format!(" ({} shown)", thousands(shown as u64)),
                theme::dim(),
            ));
        }
        if !app.filter.is_empty() {
            left.push(sep());
            left.push(Span::styled(
                format!("filter: {}", app.filter),
                theme::fg(ACCENT),
            ));
        }
        left.push(sep());
        left.push(Span::raw("sort: name ↑"));
        left.push(sep());
        let hidden = if app.show_hidden { "on" } else { "off" };
        left.push(Span::raw(format!("hidden: {hidden}")));
    }

    let mut right = Vec::new();
    if let Some((_, space)) = &app.space {
        let label = if app.cwd.is_network() {
            "share free  "
        } else {
            "free  "
        };
        right.push(Span::styled(label, theme::dim()));
        right.push(Span::raw(format!("{} ", human_size(space.free))));
    }
    bar(f, area, left, right);
}

pub fn draw_hints(f: &mut Frame, area: Rect) {
    let mut left = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, what) in HINTS {
        let w = width(key) + 1 + width(what) + 3;
        if used + w > area.width as usize {
            break;
        }
        used += w;
        left.push(Span::styled(key.to_string(), theme::bold(FG)));
        left.push(Span::styled(format!(" {what}   "), theme::dim()));
    }
    bar(f, area, left, Vec::new());
}
