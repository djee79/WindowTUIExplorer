//! Miller columns: parent | current | preview, as bordered panels (docs/screens/01-main.txt).
//!
//! Only the visible window of each column is turned into lines, so a 50k-entry
//! directory costs no more to draw than a 50-entry one.

use super::theme::{self, BORDER, DIMMED, DIR, ERR, FG, INACTIVE_SEL_BG, NET, SEL_BG};
use super::{human_size, spinner, thousands, truncate, width};
use crate::app::{App, DirState, visible_indices};
use crate::fs::worker::ListError;
use crate::fs::{Entry, EntryKind, Listing};
use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use std::time::{Instant, SystemTime};

/// Rows kept between the cursor and the panel edge while scrolling.
const SCROLLOFF: usize = 3;
/// Size and date columns in the current panel.
const SIZE_W: usize = 9;
const DATE_W: usize = 16;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let [parent, current, preview] = Layout::horizontal([
        Constraint::Fill(15),
        Constraint::Fill(31),
        Constraint::Fill(34),
    ])
    .areas(area);
    draw_parent(f, parent, app);
    draw_current(f, current, app);
    draw_preview(f, preview, app);
}

fn panel<'a>(title: String, focused: bool) -> Block<'a> {
    let title_style = if focused {
        theme::bold(theme::ACCENT)
    } else {
        theme::fg(FG)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER));
    if title.is_empty() {
        return block;
    }
    // `╭─ title ───`, like the mockups (ratatui would put it right on the corner).
    block.title(Line::from(vec![
        Span::styled("─ ", theme::fg(BORDER)),
        Span::styled(title, title_style),
        Span::raw(" "),
    ]))
}

fn draw_parent(f: &mut Frame, area: Rect, app: &App) {
    let Some(parent) = app.cwd.parent() else {
        f.render_widget(panel(String::new(), false), area);
        return;
    };
    let block = panel(parent.name(), false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(state) = app.dir(&parent) else {
        return;
    };
    let Some(listing) = state.listing.as_deref() else {
        draw_state(f, inner, app, state, false);
        return;
    };
    let rows = visible_indices(listing, app.show_hidden, "");
    let name = app.cwd.name();
    let sel = rows
        .iter()
        .position(|&i| listing.entries[i].name.eq_ignore_ascii_case(&name));
    let offset = centered_offset(sel.unwrap_or(0), rows.len(), inner.height as usize);
    draw_simple_rows(f, inner, listing, &rows, offset, sel);
}

fn draw_current(f: &mut Frame, area: Rect, app: &mut App) {
    let count = app.view.len();
    let mut block = panel(app.cwd.name(), true);
    if app.view_listing.is_some() {
        let noun = if app.filter.is_empty() {
            "items"
        } else {
            "matches"
        };
        let label = format!(" {} {noun} ", thousands(count as u64));
        let title = Line::from(vec![
            Span::styled(label, theme::dim()),
            Span::styled("─", theme::fg(BORDER)),
        ]);
        block = block.title(title.right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    // A failed refresh means the place is unreachable now: say so, even over a
    // stale listing.
    if let Some(state) = app
        .dir(&app.cwd)
        .filter(|s| s.error.is_some() || s.listing.is_none())
    {
        draw_state(f, inner, app, state, true);
        return;
    }
    let Some(listing) = app.view_listing.clone() else {
        return;
    };
    if inner.height < 2 {
        return;
    }
    let w = inner.width as usize;
    let cols = Cols::fit(w);
    let header = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(cols.header(w)).style(theme::dim()), header);

    let mut body = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    if count == 0 {
        let msg = if app.filter.is_empty() {
            " empty"
        } else {
            " no match"
        };
        f.render_widget(Paragraph::new(Span::styled(msg, theme::dim())), body);
        return;
    }
    // Footer row with the visible range, only when the list overflows.
    let overflow = count > body.height as usize;
    if overflow && body.height >= 2 {
        body.height -= 1;
    }
    app.page = body.height as usize;
    app.offset = scrolled_offset(app.cursor, app.offset, count, body.height as usize);

    let lines: Vec<Line> = app
        .view
        .iter()
        .enumerate()
        .skip(app.offset)
        .take(body.height as usize)
        .map(|(row, &i)| cols.row(&listing.entries[i], w, row == app.cursor))
        .collect();
    f.render_widget(Paragraph::new(lines), body);

    if overflow {
        let first = app.offset + 1;
        let last = (app.offset + body.height as usize).min(count);
        let range = format!("{first}–{last} of {} ", thousands(count as u64));
        let pad = w.saturating_sub(2 + width(&range));
        let footer = Rect {
            y: body.y + body.height,
            height: 1,
            ..inner
        };
        let line = Line::from(format!(" …{}{range}", " ".repeat(pad)));
        f.render_widget(Paragraph::new(line).style(theme::dim()), footer);
    }
}

fn draw_preview(f: &mut Frame, area: Rect, app: &App) {
    let entry = app.selected();
    let title = entry.map(|e| e.name.clone()).unwrap_or_default();
    let block = panel(title, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(entry) = entry else { return };

    if let EntryKind::Drive(kind) = entry.kind {
        let lines = vec![
            Line::default(),
            info_line("Type", format!("drive · {}", kind.label())),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if !entry.is_container() {
        draw_file_info(f, inner, app, entry);
        return;
    }
    let Some(loc) = app.preview_loc() else { return };
    let Some(state) = app.dir(&loc) else {
        // Not requested yet (debounce): show nothing rather than a flicker.
        return;
    };
    let Some(listing) = state.listing.as_deref().filter(|_| state.error.is_none()) else {
        draw_state(f, inner, app, state, false);
        return;
    };
    let rows = visible_indices(listing, app.show_hidden, "");
    if rows.is_empty() {
        f.render_widget(Paragraph::new(Span::styled(" empty", theme::dim())), inner);
        return;
    }
    let sel = app.remembered(&loc).and_then(|name| {
        rows.iter()
            .position(|&i| listing.entries[i].name.eq_ignore_ascii_case(name))
    });
    let offset = centered_offset(sel.unwrap_or(0), rows.len(), inner.height as usize);
    draw_simple_rows(f, inner, listing, &rows, offset, sel);
}

/// Loading / error placeholder for a panel without a usable listing.
fn draw_state(f: &mut Frame, area: Rect, app: &App, state: &DirState, primary: bool) {
    let mut lines = Vec::new();
    if let Some(err) = &state.error {
        let color = match err {
            ListError::AccessDenied | ListError::NotFound | ListError::NotADirectory => ERR,
            _ => theme::WARN,
        };
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {err}"),
            theme::fg(color),
        )));
        if primary {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                " R retry   h go back",
                theme::dim(),
            )));
        }
    } else if let Some(loading) = state.loading {
        let mut text = format!(" {} loading…", spinner(app));
        if loading.count > 0 {
            text.push_str(&format!(" {} entries", thousands(loading.count as u64)));
        }
        let waited = Instant::now().duration_since(loading.since);
        if waited.as_millis() >= 1000 {
            text.push_str(&format!("  {:.1} s", waited.as_secs_f32()));
        }
        lines.push(Line::from(Span::styled(text, theme::dim())));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Name-only rows for the side panels.
fn draw_simple_rows(
    f: &mut Frame,
    area: Rect,
    listing: &Listing,
    rows: &[usize],
    offset: usize,
    selected: Option<usize>,
) {
    let w = area.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(area.height as usize)
        .map(|(row, &i)| {
            let e = &listing.entries[i];
            let name = display_name(e, w.saturating_sub(2));
            let pad = w.saturating_sub(1 + width(&name));
            let mut style = entry_style(e);
            if selected == Some(row) {
                style = style.bg(INACTIVE_SEL_BG);
            }
            Line::from(format!(" {name}{}", " ".repeat(pad))).style(style)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// `name\` for directories, `name →` for links, truncated to `max` cells.
fn display_name(e: &Entry, max: usize) -> String {
    let suffix = match (e.kind, e.is_link) {
        (EntryKind::Dir, false) => "\\",
        (EntryKind::Dir, true) => "\\ →",
        (_, true) => " →",
        _ => "",
    };
    let name = truncate(&e.name, max.saturating_sub(width(suffix)));
    format!("{name}{suffix}")
}

fn entry_style(e: &Entry) -> Style {
    let color: Color = match e.kind {
        _ if e.is_hidden() => DIMMED,
        EntryKind::Drive(_) => theme::ACCENT,
        EntryKind::Dir if e.is_link => NET,
        EntryKind::Dir => DIR,
        EntryKind::File if e.is_link => NET,
        EntryKind::File => FG,
    };
    theme::fg(color)
}

/// Which optional columns fit in the current panel.
struct Cols {
    size: bool,
    date: bool,
}

impl Cols {
    fn fit(w: usize) -> Cols {
        Cols {
            size: w >= 24,
            date: w >= 24 + DATE_W + 2,
        }
    }

    fn right_width(&self) -> usize {
        let mut n = 1;
        if self.size {
            n += SIZE_W;
        }
        if self.date {
            n += 2 + DATE_W;
        }
        n
    }

    fn header(&self, w: usize) -> Line<'static> {
        let mut right = String::new();
        if self.size {
            right.push_str(&format!("{:>SIZE_W$}", "Size"));
        }
        if self.date {
            right.push_str(&format!("  {:<DATE_W$}", "Modified"));
        }
        right.push(' ');
        let pad = w.saturating_sub(5 + right.len());
        Line::from(format!(" Name{}{right}", " ".repeat(pad)))
    }

    fn row(&self, e: &Entry, w: usize, is_cursor: bool) -> Line<'static> {
        let name = display_name(e, w.saturating_sub(self.right_width() + 2));
        let pad = w.saturating_sub(1 + width(&name) + self.right_width());
        let mut name_style = entry_style(e);
        let mut meta_style = if e.is_hidden() {
            theme::fg(DIMMED)
        } else {
            theme::fg(FG)
        };
        let mut date_style = theme::dim();
        let mut fill = Style::new();
        if is_cursor {
            let sel = |s: Style| s.bg(SEL_BG).add_modifier(Modifier::BOLD);
            name_style = sel(name_style);
            meta_style = sel(meta_style.fg(theme::SEL_FG));
            date_style = sel(date_style);
            fill = fill.bg(SEL_BG);
        }
        let mut spans = vec![
            Span::styled(" ", fill),
            Span::styled(name, name_style),
            Span::styled(" ".repeat(pad), fill),
        ];
        if self.size {
            let size = match e.kind {
                EntryKind::File => human_size(e.size),
                EntryKind::Drive(kind) => truncate(kind.label(), SIZE_W),
                EntryKind::Dir => String::new(),
            };
            spans.push(Span::styled(format!("{size:>SIZE_W$}"), meta_style));
        }
        if self.date {
            let date = e.modified.map(fmt_minute).unwrap_or_default();
            spans.push(Span::styled(format!("  {date:<DATE_W$}"), date_style));
        }
        spans.push(Span::styled(" ", fill));
        Line::from(spans)
    }
}

fn fmt_minute(t: SystemTime) -> String {
    DateTime::<Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn fmt_second(t: SystemTime) -> String {
    DateTime::<Local>::from(t)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn info_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<10}"), theme::dim()),
        Span::styled(value, theme::fg(FG)),
    ])
}

fn draw_file_info(f: &mut Frame, area: Rect, app: &App, e: &Entry) {
    let ext = e
        .name
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
        .map(|(_, ext)| format!(" · .{}", ext.to_lowercase()))
        .unwrap_or_default();
    let size = if e.size >= 1024 {
        format!("{} bytes  ({})", thousands(e.size), human_size(e.size))
    } else {
        format!("{} bytes", e.size)
    };
    let mut lines = vec![
        Line::default(),
        info_line("Type", format!("file{ext}")),
        info_line("Size", size),
    ];
    if let Some(m) = e.modified {
        lines.push(info_line("Modified", fmt_second(m)));
    }
    if let Some(c) = e.created {
        lines.push(info_line("Created", fmt_second(c)));
    }
    lines.push(info_line("Attrib", attr_letters(e)));
    if e.is_link {
        lines.push(info_line(
            "Link",
            "symlink or junction (not followed)".to_string(),
        ));
    }
    lines.push(Line::default());
    let w = (area.width as usize).saturating_sub(12);
    let dir = format!("{}\\", app.cwd.display().trim_end_matches('\\'));
    lines.push(info_line("Path", super::truncate_left(&dir, w)));
    lines.push(info_line("", truncate(&e.name, w)));
    f.render_widget(Paragraph::new(lines), area);
}

/// Explorer-style attribute letters: `R H S A`.
fn attr_letters(e: &Entry) -> String {
    let a = e.attrs;
    let letters: Vec<&str> = [
        (a.readonly, "R"),
        (a.hidden, "H"),
        (a.system, "S"),
        (a.archive, "A"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, l)| *l)
    .collect();
    if letters.is_empty() {
        "—".to_string()
    } else {
        letters.join(" ")
    }
}

/// Keeps `cursor` in view with SCROLLOFF rows of context, moving `offset` as little as
/// possible.
fn scrolled_offset(cursor: usize, offset: usize, len: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let so = SCROLLOFF.min(height.saturating_sub(1) / 2);
    let mut off = offset;
    if cursor < off + so {
        off = cursor.saturating_sub(so);
    }
    if cursor + so >= off + height {
        off = cursor + so + 1 - height;
    }
    off.min(len.saturating_sub(height))
}

/// For side panels: show `sel` roughly centered.
fn centered_offset(sel: usize, len: usize, height: usize) -> usize {
    sel.saturating_sub(height / 2)
        .min(len.saturating_sub(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling() {
        // (cursor, offset, len, height) -> offset
        type Case = ((usize, usize, usize, usize), usize);
        let cases: &[Case] = &[
            ((0, 0, 100, 10), 0),
            ((6, 0, 100, 10), 0),
            ((7, 0, 100, 10), 1),
            ((50, 0, 100, 10), 44),
            ((44, 44, 100, 10), 41),
            ((99, 0, 100, 10), 90),
            ((3, 0, 5, 10), 0),
            ((0, 0, 0, 0), 0),
        ];
        for ((c, o, l, h), want) in cases {
            assert_eq!(scrolled_offset(*c, *o, *l, *h), *want, "{c} {o} {l} {h}");
        }
    }
}
