//! Top bar (badge + path) and the two-line footer (status, key hints),
//! per docs/screens/01-main.txt. The jobs line joins the footer in M4.

use super::theme::{self, ACCENT, BAR_BG, ERR, FG, NET, OK, ON_ACCENT};
use super::{human_size, spinner, thousands, truncate_left, width};
use crate::app::{App, Mode, Prompt, PromptKind};
use crate::fs::path::{FsRoot, Loc};
use crate::keymap::fmt_keys;
use crate::prompt::EditMode;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Keys shown in the hint line. Only what exists; it grows with each milestone.
const HINTS: &[(&str, &str)] = &[
    ("hjkl", "move"),
    ("/", "search"),
    ("f", "filter"),
    ("H/L", "history"),
    ("''", "previous dir"),
    ("gl", "Locations"),
    ("g.", "hidden"),
    ("<Space>", "leader"),
    (":", "cmd"),
    ("ZZ", "quit + cd"),
    ("g?", "help"),
];

const ROOT_HINTS: &[(&str, &str)] = &[
    ("l/Enter", "open"),
    ("R", "re-probe"),
    ("dd", "remove share"),
    ("''", "previous dir"),
    (":cd", "\\\\server\\share"),
    ("-", "parent"),
    ("g?", "help"),
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
    if app.at_root()
        && let Some(s) = super::locations::reach_summary(app)
    {
        right.push(Span::styled(format!("{s}  "), theme::dim()));
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

fn badge(text: &'static str, bg: ratatui::style::Color) -> Span<'static> {
    Span::styled(
        text,
        Style::new()
            .bg(bg)
            .fg(ON_ACCENT)
            .add_modifier(Modifier::BOLD),
    )
}

/// `:cd C:\wo█` with the cursor as a reversed cell, then the prompt's note.
fn prompt_spans(p: &Prompt) -> Vec<Span<'static>> {
    let text: Vec<char> = p.editor.text().chars().collect();
    let at = p.editor.cursor().min(text.len());
    let before: String = text[..at].iter().collect();
    let (under, after) = match text.get(at) {
        Some(c) => (c.to_string(), text[at + 1..].iter().collect::<String>()),
        None => (" ".to_string(), String::new()),
    };
    let mut spans = vec![
        Span::styled(p.prefix(), theme::bold(ACCENT)),
        Span::styled(before, theme::fg(FG)),
        Span::styled(under, Style::new().fg(BAR_BG).bg(FG)),
        Span::styled(after, theme::fg(FG)),
    ];
    if !p.note.is_empty() {
        spans.push(Span::styled(format!("   {}", p.note), theme::dim()));
    }
    if p.kind == PromptKind::Filter {
        spans.push(Span::styled("   Enter keep · Esc Esc clear", theme::dim()));
    }
    spans
}

pub fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let sep = || Span::styled("  ·  ", theme::dim());
    let mut left: Vec<Span> = vec![Span::raw(" ")];
    if let (Mode::Prompt, Some(p)) = (app.mode, &app.prompt) {
        left.push(match p.editor.mode {
            EditMode::Insert => badge(" INSERT ", OK),
            EditMode::Normal => badge(" NORMAL ", ACCENT),
        });
        left.push(Span::raw("  "));
        left.extend(prompt_spans(p));
        bar(f, area, left, Vec::new());
        return;
    }
    left.push(badge(" NORMAL ", ACCENT));
    left.push(Span::raw("  "));
    if let Some(status) = &app.status {
        let color = if status.is_error { ERR } else { FG };
        left.push(Span::styled(status.text.clone(), theme::fg(color)));
    } else if app.at_root() {
        left.push(Span::raw(super::locations::summary(app)));
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
    // vim's showcmd: the count and keys typed so far.
    let pending = format!(
        "{}{}",
        app.count.map(|c| c.to_string()).unwrap_or_default(),
        fmt_keys(&app.keys_pending, app.keymap.leader)
    );
    if !pending.is_empty() {
        right.push(Span::styled(format!("{pending}   "), theme::bold(ACCENT)));
    }
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

pub fn draw_hints(f: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![Span::raw(" ")];
    let mut used = 1;
    let hints = if app.at_root() { ROOT_HINTS } else { HINTS };
    for (key, what) in hints {
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
