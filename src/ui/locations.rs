//! The Locations root as its own screen (docs/screens/02-locations.txt): a sectioned list
//! (PINNED FOLDERS, DRIVES, SHARES) with reachability dots, and a detail panel.

use super::columns::{draw_state, fmt_second, info_line, panel, scrolled_offset};
use super::theme::{self, DIM, ERR, FG, NET, OK, SEL_BG};
use super::{human_size, spinner, truncate, truncate_left, width};
use crate::app::{App, REPROBE_EVERY, Reach};
use crate::fs::worker::{ListError, PROBE_TIMEOUT};
use crate::fs::{Entry, EntryKind};
use crate::platform::DriveKind;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// `C:  ` / `'a  ` column, then the name column, as in the mockup.
const KEY_W: usize = 4;
const NAME_W: usize = 19;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Pinned,
    Drives,
    Shares,
    Machines,
}

impl Section {
    fn of(e: &Entry) -> Section {
        match e.kind {
            EntryKind::Share => Section::Shares,
            EntryKind::Pin => Section::Pinned,
            EntryKind::Machine(_) => Section::Machines,
            _ => Section::Drives,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Section::Pinned => "PINNED FOLDERS",
            Section::Drives => "DRIVES",
            Section::Shares => "SHARES",
            Section::Machines => "MACHINES",
        }
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let [list, detail] =
        Layout::horizontal([Constraint::Fill(86), Constraint::Fill(74)]).areas(area);
    draw_list(f, list, app);
    draw_detail(f, detail, app);
}

fn draw_list(f: &mut Frame, area: Rect, app: &mut App) {
    let block = panel("Locations".to_string(), true);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(state) = app
        .dir(&app.cwd)
        .filter(|s| s.listing.is_none() || s.error.is_some())
    {
        draw_state(f, inner, app, state, true);
        return;
    }
    let Some(listing) = app.view_listing.clone() else {
        return;
    };

    // Lines with section headers in between; remember where the cursor row lands.
    let w = inner.width as usize;
    let mut lines: Vec<Line> = vec![Line::default()];
    let mut cursor_line = 0;
    let mut current: Option<Section> = None;
    if app.filter.is_empty() && !listing.entries.iter().any(|e| e.kind == EntryKind::Pin) {
        lines.push(header(Section::Pinned));
        lines.push(Line::from(Span::styled(
            "  none yet: in a folder, m{key} pins it ('{key} jumps back); M pins the item under the cursor",
            theme::dim(),
        )));
        lines.push(Line::default());
    }
    for (row, &i) in app.view.iter().enumerate() {
        let e = &listing.entries[i];
        let section = Section::of(e);
        if current != Some(section) {
            if current.is_some() {
                lines.push(Line::default());
            }
            lines.push(header(section));
            current = Some(section);
        }
        if row == app.cursor {
            cursor_line = lines.len();
        }
        lines.push(row_line(app, e, w, row == app.cursor));
    }
    if app.view.is_empty() && !app.filter.is_empty() {
        lines.push(Line::from(Span::styled("  no match", theme::dim())));
    }
    if current != Some(Section::Shares) && app.filter.is_empty() {
        lines.push(Line::default());
        lines.push(header(Section::Shares));
        lines.push(Line::from(Span::styled(
            "  none yet: browse a \\\\server\\share once and it appears here",
            theme::dim(),
        )));
    }

    // Legend pinned to the bottom when there's room.
    let mut body = inner;
    if body.height > 8 {
        body.height -= 2;
        let legend = Rect {
            y: body.y + body.height,
            height: 1,
            ..inner
        };
        f.render_widget(Paragraph::new(legend_line(w)), legend);
    }
    let h = body.height as usize;
    app.page = h;
    app.offset = scrolled_offset(cursor_line, app.offset, lines.len(), h);
    let visible: Vec<Line> = lines.into_iter().skip(app.offset).take(h).collect();
    f.render_widget(Paragraph::new(visible), body);
}

fn header(s: Section) -> Line<'static> {
    Line::from(Span::styled(format!("  {}", s.title()), theme::bold(DIM)))
}

fn legend_line(w: usize) -> Line<'static> {
    let left = "  ● reachable   ○ not checked yet   × unreachable";
    let right = format!("re-probed every {} s ", REPROBE_EVERY.as_secs());
    let pad = w.saturating_sub(width(left) + width(&right));
    Line::from(vec![
        Span::styled("  ", theme::dim()),
        Span::styled("●", theme::fg(OK)),
        Span::styled(" reachable   ", theme::dim()),
        Span::styled("○", theme::dim()),
        Span::styled(" not checked yet   ", theme::dim()),
        Span::styled("×", theme::fg(ERR)),
        Span::styled(" unreachable", theme::dim()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, theme::dim()),
    ])
}

/// The dot for an entry: ● ok, × failed, ○ never checked, spinner while the first probe
/// runs. A re-probe keeps showing the last answer.
fn dot(app: &App, reach: Option<&Reach>) -> Span<'static> {
    match reach {
        Some(r) if r.is_reachable() => Span::styled("●", theme::fg(OK)),
        Some(r) if r.is_unreachable() => Span::styled("×", theme::fg(ERR)),
        Some(r) if r.probing => Span::styled(spinner(app), theme::dim()),
        _ => Span::styled("○", theme::dim()),
    }
}

/// Short right-column text for a failed probe.
fn short_error(e: &Entry, err: &ListError) -> &'static str {
    match (e.kind, err) {
        (EntryKind::Drive(DriveKind::Network), _) => "disconnected",
        (_, ListError::TimedOut) => "timed out",
        (_, ListError::AccessDenied) => "access denied",
        (_, ListError::NotFound) => "not found",
        (_, ListError::Unreachable(_)) => "unreachable",
        _ => "error",
    }
}

/// `  ● JDOE-LT042              online · 4 ms` (the dot from the port-445 probe).
fn machine_line(e: &Entry, w: usize, is_cursor: bool, online: Option<bool>) -> Line<'static> {
    let base = if is_cursor {
        Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let (dot, color) = match online {
        Some(true) => ("●", OK),
        Some(false) => ("×", ERR),
        None => ("○", DIM),
    };
    let name_w = KEY_W + NAME_W;
    let right = truncate(&e.detail, w.saturating_sub(name_w + 6));
    let pad = w.saturating_sub(4 + name_w + width(&right) + 1);
    Line::from(vec![
        Span::styled("  ", base),
        Span::styled(dot, base.fg(color)),
        Span::styled(" ", base),
        Span::styled(
            format!("{:<name_w$}", truncate(&e.name, name_w - 1)),
            base.fg(FG),
        ),
        Span::styled(" ".repeat(pad), base),
        Span::styled(right, base.fg(DIM)),
        Span::styled(" ", base),
    ])
}

fn row_line(app: &App, e: &Entry, w: usize, is_cursor: bool) -> Line<'static> {
    if let EntryKind::Machine(online) = e.kind {
        return machine_line(e, w, is_cursor, online);
    }
    let reach = App::root_target(e).and_then(|t| app.reach_of(&t).cloned());
    let (key, name, path, name_color) = match e.kind {
        EntryKind::Drive(DriveKind::Network) => {
            (e.name.clone(), "→".to_string(), e.detail.clone(), NET)
        }
        EntryKind::Drive(_) => (e.name.clone(), e.detail.clone(), String::new(), FG),
        EntryKind::Pin => {
            let key = app
                .pin_of(e)
                .and_then(|p| p.key_char())
                .map(|c| format!("'{c}"))
                .unwrap_or_default();
            (key, e.detail.clone(), e.name.clone(), FG)
        }
        _ => (String::new(), String::new(), e.name.clone(), NET),
    };
    let right = match (&reach.as_ref().and_then(|r| r.last.clone()), e.kind) {
        (Some(Err(err)), _) => short_error(e, err).to_string(),
        (_, EntryKind::Drive(DriveKind::Network)) => "mapped".to_string(),
        (Some(Ok(p)), EntryKind::Drive(_)) => p
            .free
            .map(|b| format!("{} free", human_size(b)))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let right_color = match reach.as_ref() {
        Some(r) if r.is_unreachable() => ERR,
        _ => DIM,
    };

    // "  ● C:  Windows            \\srv\share           412.0 GB free "
    let fixed = 2 + 2 + KEY_W + NAME_W;
    let right_w = width(&right) + 1;
    let path_w = w.saturating_sub(fixed + right_w + 1);
    let path = truncate_left(&path, path_w);
    let pad = w.saturating_sub(fixed + width(&path) + right_w);

    let base = if is_cursor {
        Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let st = |c| base.fg(c);
    Line::from(vec![
        Span::styled("  ", base),
        dot(app, reach.as_ref()).patch_style(base),
        Span::styled(" ", base),
        Span::styled(format!("{:<KEY_W$}", key), st(theme::ACCENT)),
        Span::styled(
            format!("{:<NAME_W$}", truncate(&name, NAME_W - 1)),
            st(name_color),
        ),
        Span::styled(path, st(NET)),
        Span::styled(" ".repeat(pad), base),
        Span::styled(right, st(right_color)),
        Span::styled(" ", base),
    ])
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let entry = app.selected().filter(|_| app.view_listing.is_some());
    let title = entry.map(|e| e.name.clone()).unwrap_or_default();
    let block = panel(title, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(e) = entry else { return };
    let target = App::root_target(e);
    let reach = target.as_ref().and_then(|t| app.reach_of(t));
    let vw = (inner.width as usize).saturating_sub(12);

    let mut lines = vec![Line::default()];
    match e.kind {
        EntryKind::Drive(DriveKind::Network) => {
            lines.push(info_line("Kind", "network drive (mapped)".to_string()));
            if !e.detail.is_empty() {
                lines.push(info_line("Target", truncate_left(&e.detail, vw)));
            }
        }
        EntryKind::Drive(kind) => {
            lines.push(info_line("Kind", kind.label().to_string()));
            if !e.detail.is_empty() {
                lines.push(info_line("Label", e.detail.clone()));
            }
        }
        EntryKind::Pin => {
            let pin = app.pin_of(e);
            let tags = pin
                .map(|p| {
                    p.tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            lines.push(info_line("Kind", "folder pin".to_string()));
            lines.push(info_line("Path", truncate_left(&e.name, vw)));
            let key = pin.and_then(|p| p.key_char());
            lines.push(info_line(
                "Key",
                key.map_or("none (m{key} in the folder sets one)".to_string(), |c| {
                    format!("'{c}")
                }),
            ));
            if !tags.is_empty() {
                lines.push(info_line("Tags", truncate(&tags, vw)));
            }
        }
        EntryKind::Machine(_) => {
            lines.push(info_line("Kind", "machine (admin shares)".to_string()));
            lines.push(info_line("Status", e.detail.clone()));
            if let Some(u) = app.machine_user(&e.name) {
                lines.push(info_line("User", u));
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                " l opens its view · M pins it · gm all machines",
                theme::dim(),
            )));
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        _ => {
            lines.push(info_line("Kind", "saved share".to_string()));
            lines.push(info_line("Path", truncate_left(&e.name, vw)));
        }
    }
    if let Some(free) = reach
        .and_then(|r| r.last.as_ref())
        .and_then(|l| l.as_ref().ok())
        .and_then(|p| p.free)
    {
        lines.push(info_line("Free", human_size(free)));
    }
    lines.push(Line::default());
    lines.extend(status_lines(app, reach));

    lines.push(Line::default());
    let mut hints = vec![
        Span::styled(" l", theme::bold(FG)),
        Span::styled(" open      ", theme::dim()),
        Span::styled("R", theme::bold(FG)),
        Span::styled(" retry now      ", theme::dim()),
    ];
    if e.kind == EntryKind::Share {
        hints.push(Span::styled("dd", theme::bold(FG)));
        hints.push(Span::styled(" remove from list", theme::dim()));
    }
    lines.push(Line::from(hints));
    if e.kind == EntryKind::Pin {
        lines.push(Line::from(vec![
            Span::styled(" r", theme::bold(FG)),
            Span::styled(" rename    ", theme::dim()),
            Span::styled("T", theme::bold(FG)),
            Span::styled(" tags           ", theme::dim()),
            Span::styled("dd", theme::bold(FG)),
            Span::styled(" unpin", theme::dim()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn status_lines(app: &App, reach: Option<&Reach>) -> Vec<Line<'static>> {
    let Some(r) = reach else {
        return vec![info_line("Status", "○ not checked yet".to_string())];
    };
    let at = r
        .checked_at
        .map(|t| format!("  (probe at {})", &fmt_second(t)[11..]))
        .unwrap_or_default();
    let mut lines = Vec::new();
    match &r.last {
        None => lines.push(Line::from(vec![
            Span::styled(format!(" {:<10}", "Status"), theme::dim()),
            Span::styled(format!("{} probing…", spinner(app)), theme::dim()),
        ])),
        Some(Ok(p)) => lines.push(Line::from(vec![
            Span::styled(format!(" {:<10}", "Status"), theme::dim()),
            Span::styled("● reachable", theme::fg(OK)),
            Span::styled(format!("  ·  {} ms{at}", p.rtt.as_millis()), theme::dim()),
        ])),
        Some(Err(err)) => {
            lines.push(Line::from(vec![
                Span::styled(format!(" {:<10}", "Status"), theme::dim()),
                Span::styled("× unreachable", theme::fg(ERR)),
            ]));
            let why = match err {
                ListError::TimedOut => {
                    format!("timed out after {:.1} s", PROBE_TIMEOUT.as_secs_f32())
                }
                other => other.to_string(),
            };
            lines.push(info_line("", format!("{why}{at}")));
            if let Some((t, rtt)) = r.last_ok {
                lines.push(info_line(
                    "Last ok",
                    format!("{}  ·  {} ms", fmt_second(t), rtt.as_millis()),
                ));
            }
            lines.push(Line::default());
            for text in [
                "The UI never waits on this location. Opening it",
                "shows this message and stays where you are.",
            ] {
                lines.push(Line::from(Span::styled(format!(" {text}"), theme::dim())));
            }
        }
    }
    if r.probing && r.last.is_some() {
        lines.push(info_line("", format!("{} re-probing…", spinner(app))));
    }
    lines
}

/// Status-line summary: `7 locations · 4 drives · 3 shares`.
pub fn summary(app: &App) -> String {
    let Some(listing) = app.view_listing.as_ref() else {
        return String::new();
    };
    let count = |k: EntryKind| listing.entries.iter().filter(|e| e.kind == k).count();
    let (shares, pins) = (count(EntryKind::Share), count(EntryKind::Pin));
    let drives = listing.entries.len() - shares - pins;
    format!(
        "{} locations  ·  {pins} pinned folders  ·  {drives} drives  ·  {shares} shares",
        listing.entries.len()
    )
}

/// Top-bar summary: `3 of 4 reachable`, over entries probed so far.
pub fn reach_summary(app: &App) -> Option<String> {
    let listing = app.view_listing.as_ref()?;
    let (mut ok, mut checked) = (0, 0);
    for t in listing.entries.iter().filter_map(App::root_target) {
        match app.reach_of(&t) {
            Some(r) if r.is_reachable() => {
                ok += 1;
                checked += 1;
            }
            Some(r) if r.is_unreachable() => checked += 1,
            _ => {}
        }
    }
    (checked > 0).then(|| format!("{ok} of {checked} reachable"))
}
