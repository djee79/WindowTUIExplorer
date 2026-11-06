//! Top bar (badge + path) and the two-line footer (status, key hints),
//! per docs/screens/01-main.txt. The jobs line joins the footer in M4.

use super::theme::{self, ACCENT, BAR_BG, ERR, FG, NET, OK, ON_ACCENT};
use super::{human_size, spinner, thousands, truncate_left, width};
use crate::app::{App, Mode, Prompt, PromptKind, SearchState};
use std::time::Duration;

/// A running search that hasn't moved this long gets a warning (rule 2's network timeout).
const STALL_AFTER: Duration = Duration::from_secs(3);
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
    ("/", "find here"),
    ("s", "search below"),
    ("S", "grep below"),
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

const GREP_HINTS: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("]m/[m", "step matches"),
    ("l", "open"),
    ("gr", "go to folder"),
    ("S", "refine"),
    (",s", "sort"),
    ("H/Esc", "back"),
    ("g?", "help"),
];

const RESULT_HINTS: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("l", "open"),
    ("gr", "go to folder"),
    ("s", "refine"),
    (",s", "sort"),
    ("f", "filter"),
    ("H/Esc", "back"),
    ("g?", "help"),
];

const PICKER_INSERT_HINTS: &[(&str, &str)] = &[
    ("^j/^k", "move"),
    ("Enter", "open"),
    ("#", "tag filter"),
    ("Tab", "complete"),
    ("^e/^t/^x", "edit · tags · delete"),
    ("Esc", "normal mode"),
];

const PICKER_NORMAL_HINTS: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("Enter", "open"),
    ("]t/[t", "cycle tag"),
    ("i", "type"),
    ("^e/^t/^x", "edit · tags · delete"),
    ("q/Esc", "close"),
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

/// `search "q"  in  \\srv\share\HIT` … `● 48 213 scanned · 9 found · 2.1 s`.
fn draw_search_header(f: &mut Frame, area: Rect, app: &App, s: &SearchState) {
    let mut right: Vec<Span> = Vec::new();
    let stalled = s.running() && s.last_progress.elapsed() >= STALL_AFTER;
    if s.running() {
        right.push(Span::styled(
            format!("{}  ", spinner(app)),
            theme::fg(ACCENT),
        ));
    }
    if stalled {
        right.push(Span::styled(
            format!(
                "⚠ no answer for {:.0} s   ",
                s.last_progress.elapsed().as_secs_f32()
            ),
            theme::fg(theme::WARN),
        ));
    }
    right.push(Span::styled(
        format!(
            "{} scanned · {} found · {:.1} s  ",
            thousands(s.scanned),
            thousands(s.hits.len() as u64),
            s.took().as_secs_f32()
        ),
        theme::dim(),
    ));
    let rw: usize = right.iter().map(|s| s.width()).sum();
    let origin = truncate_left(
        &s.origin.display(),
        (area.width as usize).saturating_sub(rw + 24 + width(&s.query)),
    );
    let color = if s.origin.is_network() { NET } else { FG };
    let left = vec![
        badge(" sentier ", ACCENT),
        Span::raw("  "),
        Span::styled(if s.content { "grep " } else { "search " }, theme::dim()),
        Span::styled(format!("\"{}\"", s.query), theme::bold(ACCENT)),
        Span::styled("  in  ", theme::dim()),
        Span::styled(origin, theme::bold(color)),
    ];
    bar(f, area, left, right);
}

pub fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    if let Some(s) = app.search_state(&app.cwd) {
        return draw_search_header(f, area, app, s);
    }
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
    match loc.real() {
        Loc::Server(s) => Some(s.clone()),
        Loc::Fs(p) => match p.root() {
            FsRoot::Share { server, .. } => Some(server.clone()),
            FsRoot::Drive(_) => None,
        },
        Loc::Root | Loc::Results { .. } => None,
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
    if let (Mode::Picker, Some(p)) = (app.mode, &app.picker) {
        left.push(match p.mode {
            crate::picker::PickerMode::Insert => badge(" INSERT ", OK),
            crate::picker::PickerMode::Normal => badge(" NORMAL ", ACCENT),
        });
        left.push(Span::raw("  "));
        let msg = match &app.status {
            Some(s) => Span::styled(s.text.clone(), theme::fg(if s.is_error { ERR } else { FG })),
            None => Span::styled(
                format!("pins  ·  one store: pins.toml  ·  {} pins", p.total()),
                theme::dim(),
            ),
        };
        left.push(msg);
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
    } else if let Some(s) = app.search_state(&app.cwd) {
        left.push(Span::raw(if s.content {
            "content search results"
        } else {
            "search results"
        }));
        left.push(sep());
        left.push(Span::raw(format!(
            "{} found",
            thousands(s.hits.len() as u64)
        )));
        if app.view.len() != s.hits.len() {
            left.push(Span::styled(
                format!(" ({} shown)", thousands(app.view.len() as u64)),
                theme::dim(),
            ));
        }
        left.push(sep());
        left.push(Span::raw(format!("sort: {}", s.sort_label())));
        if s.running() && !s.current.is_empty() {
            left.push(sep());
            left.push(Span::styled(
                format!("walking {}", truncate_left(&s.current, 48)),
                theme::dim(),
            ));
        }
        if s.truncated {
            left.push(sep());
            left.push(Span::styled(
                "capped: refine the query",
                theme::fg(theme::WARN),
            ));
        }
        let hint = if s.running() {
            "Esc cancels the walk, keeps the results "
        } else {
            "Esc / H back to the folder "
        };
        bar(f, area, left, vec![Span::styled(hint, theme::dim())]);
        return;
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
    let picker_mode = app
        .picker
        .as_ref()
        .filter(|_| app.mode == Mode::Picker)
        .map(|p| p.mode);
    let hints = if let Some(m) = picker_mode {
        match m {
            crate::picker::PickerMode::Insert => PICKER_INSERT_HINTS,
            crate::picker::PickerMode::Normal => PICKER_NORMAL_HINTS,
        }
    } else if app.at_root() {
        ROOT_HINTS
    } else if let Some(s) = app.search_state(&app.cwd) {
        if s.content { GREP_HINTS } else { RESULT_HINTS }
    } else {
        HINTS
    };
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
