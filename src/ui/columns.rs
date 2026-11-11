//! Miller columns: parent | current | preview, as bordered panels (docs/screens/01-main.txt).
//!
//! Only the visible window of each column is turned into lines, so a 50k-entry
//! directory costs no more to draw than a 50-entry one.

use super::theme;
use super::{human_size, spinner, thousands, truncate, width};
use crate::app::{App, DirState, Mode, visible_indices};
use crate::fs::worker::ListError;
use crate::fs::{Entry, EntryKind, Listing, Template};
use crate::preview::PreviewKind;
use crate::preview::image::{ImagePreview, Picture};
use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui_image::Image;
use std::sync::Arc;
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
    draw_preview_panel(f, preview, app);
}

/// `zp`: the preview panel over the whole body.
pub fn draw_fullscreen(f: &mut Frame, area: Rect, app: &mut App) {
    draw_preview_panel(f, area, app);
}

/// The preview panel in `area`: tells the app the box a picture may fill (the panel less a
/// margin and two caption lines) and where one went.
fn draw_preview_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = panel(String::new(), false).inner(area);
    app.image_area = Some(Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(2),
        ..inner
    });
    let drawn = draw_preview(f, area, app);
    app.image_drawn = drawn.as_ref().map(|d| d.rect);
    app.image_place = drawn.and_then(|d| d.place);
}

pub(super) fn panel<'a>(title: String, focused: bool) -> Block<'a> {
    let title_style = if focused {
        theme::bold(theme::pal().accent)
    } else {
        theme::fg(theme::pal().fg)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(theme::pal().border));
    if title.is_empty() {
        return block;
    }
    // `╭─ title ───`, like the mockups (ratatui would put it right on the corner).
    block.title(Line::from(vec![
        Span::styled("─ ", theme::fg(theme::pal().border)),
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
    let search = app.search_state(&app.cwd);
    let title = match search {
        Some(s) if s.content => format!("grep: {}", s.query),
        Some(s) => format!("search: {}", s.query),
        None => app.cwd.name(),
    };
    let mut block = panel(title, true);
    if app.view_listing.is_some() {
        let noun = match (search.is_some(), app.filter.is_empty()) {
            (true, true) => "found",
            (false, true) => "items",
            (_, false) => "matches",
        };
        let mut spans = vec![Span::styled(
            format!(" {} {noun} ", thousands(count as u64)),
            theme::dim(),
        )];
        if let Some(s) = search {
            // ● while the walk runs, ✓ when it finished, × when cancelled.
            let (mark, color) = match (s.running(), s.cancelled) {
                (true, _) => ("● ", theme::pal().accent),
                (false, true) => ("× ", theme::pal().warn),
                (false, false) => ("✓ ", theme::pal().ok),
            };
            spans.push(Span::styled(mark, theme::fg(color)));
        }
        spans.push(Span::styled("─", theme::fg(theme::pal().border)));
        block = block.title(Line::from(spans).right_aligned());
    }
    let in_results = search.is_some();
    let scored = search.is_some_and(|s| s.scored);
    let content = search.is_some_and(|s| s.content);
    let running = search.is_some_and(|s| s.running());
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
    let mut cols = Cols::fit(w, gutter_width(count, app.number, app.relative_number));
    cols.results = in_results;
    cols.scored = scored;
    cols.content = content;
    let header = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(cols.header(w)).style(theme::dim()), header);

    let mut body = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    if count == 0 {
        let msg = match (in_results, running, app.filter.is_empty()) {
            (true, true, _) => " searching…",
            (true, false, true) => " nothing found",
            (false, _, true) => " empty",
            (_, _, false) => " no match",
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
        .map(|(row, &i)| {
            let e = &listing.entries[i];
            let number = (cols.gutter > 0).then(|| {
                gutter_cell(
                    row,
                    app.cursor,
                    cols.gutter,
                    app.number,
                    app.relative_number,
                )
            });
            cols.row(
                e,
                w,
                row == app.cursor,
                app.is_search_match(&e.name),
                number,
                app.is_marked(row, &e.name),
            )
        })
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

/// The preview panel. Returns where a picture was drawn, if one was.
fn draw_preview(f: &mut Frame, area: Rect, app: &App) -> Option<Drawn> {
    let entry = app.selected();
    let mut title = entry.map(|e| e.name.clone()).unwrap_or_default();
    if app.mode == Mode::Preview && app.zoom() > 100 {
        title = format!("{title} · {} %", app.zoom());
    }
    let block = panel(title, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let entry = entry?;

    if let EntryKind::Drive(kind) = entry.kind {
        let lines = vec![
            Line::default(),
            info_line("Type", format!("drive · {}", kind.label())),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return None;
    }
    if !entry.is_container() {
        if app.search_state(&app.cwd).is_some_and(|s| s.content) {
            super::grep::draw(f, inner, app, entry);
        } else if let Some((tp, scroll)) = app
            .cwd
            .join(&entry.name)
            .ok()
            .and_then(|loc| app.text_of(&loc))
        {
            draw_text(f, inner, tp, scroll);
        } else if let Some(kind) = PreviewKind::of(&entry.name) {
            // Most files have no Windows thumbnail: they show their info, also while
            // the shell is being asked (no spinner flashing on every file).
            let thumbnail_ready = || {
                app.cwd
                    .join(&entry.name)
                    .ok()
                    .and_then(|loc| app.image_of(&loc))
                    .is_some_and(|s| matches!(s.result, Some(Ok(_))))
            };
            if kind != PreviewKind::Thumbnail || thumbnail_ready() {
                return draw_image(f, inner, app, entry);
            }
            draw_file_info(f, inner, app, entry);
        } else {
            draw_file_info(f, inner, app, entry);
        }
        return None;
    }
    let loc = app.preview_loc()?;
    // Not requested yet (debounce): show nothing rather than a flicker.
    let state = app.dir(&loc)?;
    let Some(listing) = state.listing.as_deref().filter(|_| state.error.is_none()) else {
        draw_state(f, inner, app, state, false);
        return None;
    };
    let rows = visible_indices(listing, app.show_hidden, "");
    if rows.is_empty() {
        f.render_widget(Paragraph::new(Span::styled(" empty", theme::dim())), inner);
        return None;
    }
    let sel = app.remembered(&loc).and_then(|name| {
        rows.iter()
            .position(|&i| listing.entries[i].name.eq_ignore_ascii_case(name))
    });
    let offset = centered_offset(sel.unwrap_or(0), rows.len(), inner.height as usize);
    draw_simple_rows(f, inner, listing, &rows, offset, sel);
    None
}

/// Loading / error placeholder for a panel without a usable listing.
pub(super) fn draw_state(f: &mut Frame, area: Rect, app: &App, state: &DirState, primary: bool) {
    let mut lines = Vec::new();
    if let Some(err) = &state.error {
        let color = match err {
            ListError::AccessDenied | ListError::NotFound | ListError::NotADirectory => {
                theme::pal().err
            }
            _ => theme::pal().warn,
        };
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {err}"),
            theme::fg(color),
        )));
        if primary {
            lines.push(Line::default());
            // On a share, logging in may be what's missing.
            let hint = if crate::auth::connect_target(&app.cwd).is_some() {
                " R retry   :connect log in   h go back"
            } else {
                " R retry   h go back"
            };
            lines.push(Line::from(Span::styled(hint, theme::dim())));
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
                style = style.bg(theme::pal().inactive_sel_bg);
            }
            Line::from(format!(" {name}{}", " ".repeat(pad))).style(style)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// `name\` for directories, `name →` for links, truncated to `max` cells.
fn display_name(e: &Entry, max: usize) -> String {
    // A template shows `d  Desktop` (its full path goes in the date column).
    if let EntryKind::Template(_) = e.kind {
        return truncate(&e.detail, max);
    }
    let suffix = match (e.kind, e.is_link) {
        (EntryKind::Dir, false) | (EntryKind::Share | EntryKind::Pin, _) => "\\",
        (EntryKind::Dir, true) => "\\ →",
        (_, true) => " →",
        _ => "",
    };
    let name = truncate(&e.name, max.saturating_sub(width(suffix)));
    format!("{name}{suffix}")
}

fn entry_style(e: &Entry) -> Style {
    let color: Color = match e.kind {
        _ if e.is_hidden() => theme::pal().dimmed,
        EntryKind::Drive(_) => theme::pal().accent,
        EntryKind::Share | EntryKind::Pin => theme::pal().net,
        EntryKind::Dir if e.is_link => theme::pal().net,
        EntryKind::Dir => theme::pal().dir,
        EntryKind::File if e.is_link => theme::pal().net,
        EntryKind::File => theme::pal().fg,
        EntryKind::Machine(Some(true)) => theme::pal().net,
        EntryKind::Machine(Some(false)) => theme::pal().dimmed,
        EntryKind::Machine(None) => theme::pal().fg,
        EntryKind::Template(Template::Missing | Template::NeedsUser) => theme::pal().dimmed,
        EntryKind::Template(_) => theme::pal().dir,
    };
    theme::fg(color)
}

/// The path of a template relative to its machine: `C$\Users\jdoe\Desktop`.
fn template_path(e: &Entry) -> String {
    let rest = e.name.trim_start_matches('\\');
    rest.split_once('\\').map_or(rest, |(_, p)| p).to_string()
}

/// Which optional columns fit in the current panel.
struct Cols {
    size: bool,
    date: bool,
    /// Width of the line-number field (0 = `nonu nornu`). It sits after the 1-cell
    /// selection-marker column and is followed by a space.
    gutter: usize,
    /// Search results: "Path" instead of "Name", and the score where dates go.
    results: bool,
    scored: bool,
    /// Content results: that number is a match count ("Matches").
    content: bool,
}

/// The number for `row` in neovim's style. Hybrid (`nu rnu`): the cursor row shows its
/// absolute number left-aligned, others their distance right-aligned. `rnu` alone shows
/// 0 on the cursor row; `nu` alone shows absolute numbers everywhere.
pub(super) fn gutter_cell(
    row: usize,
    cursor: usize,
    w: usize,
    number: bool,
    relative: bool,
) -> Span<'static> {
    let text = match (number, relative, row == cursor) {
        (true, true, true) => format!("{:<w$}", row + 1),
        (_, true, _) => format!("{:>w$}", row.abs_diff(cursor)),
        _ => format!("{:>w$}", row + 1),
    };
    let style = if row == cursor {
        theme::bold(theme::pal().accent)
    } else {
        theme::fg(theme::pal().dimmed)
    };
    Span::styled(text, style)
}

/// Gutter width for a list of `len` rows: at least 3 cells, like vim's `numberwidth`.
pub(super) fn gutter_width(len: usize, number: bool, relative: bool) -> usize {
    if !number && !relative {
        return 0;
    }
    len.max(1).to_string().len().max(3)
}

impl Cols {
    fn fit(w: usize, gutter: usize) -> Cols {
        let lead = if gutter > 0 { gutter + 1 } else { 0 };
        let rest = w.saturating_sub(lead);
        Cols {
            size: rest >= 24,
            date: rest >= 24 + DATE_W + 2,
            gutter,
            results: false,
            content: false,
            scored: false,
        }
    }

    /// Cells before the name: the marker column plus the gutter.
    fn lead(&self) -> usize {
        1 + if self.gutter > 0 { self.gutter + 1 } else { 0 }
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
            let label = if self.results && self.content {
                "Matches"
            } else if self.results && self.scored {
                "Score"
            } else {
                "Modified"
            };
            right.push_str(&format!("  {label:<DATE_W$}"));
        }
        right.push(' ');
        let lead = " ".repeat(self.lead());
        let name = if self.results { "Path" } else { "Name" };
        let pad = w.saturating_sub(self.lead() + 4 + right.len());
        Line::from(format!("{lead}{name}{}{right}", " ".repeat(pad)))
    }

    fn row(
        &self,
        e: &Entry,
        w: usize,
        is_cursor: bool,
        search_hit: bool,
        number: Option<Span<'static>>,
        marked: bool,
    ) -> Line<'static> {
        let name = display_name(e, w.saturating_sub(self.right_width() + self.lead() + 1));
        let pad = w.saturating_sub(self.lead() + width(&name) + self.right_width());
        let mut name_style = entry_style(e);
        if search_hit {
            name_style = name_style
                .fg(theme::pal().accent)
                .add_modifier(Modifier::UNDERLINED);
        }
        let mut meta_style = if e.is_hidden() {
            theme::fg(theme::pal().dimmed)
        } else {
            theme::fg(theme::pal().fg)
        };
        let mut date_style = theme::dim();
        let mut fill = Style::new();
        if is_cursor {
            let sel = |s: Style| s.bg(theme::pal().sel_bg).add_modifier(Modifier::BOLD);
            name_style = sel(name_style);
            meta_style = sel(meta_style.fg(theme::pal().sel_fg));
            date_style = sel(date_style);
            fill = fill.bg(theme::pal().sel_bg);
        }
        // Marker column (selection) and line number stay off the cursor highlight, like
        // neovim's sign column and CursorLineNr.
        let mut spans = vec![if marked {
            Span::styled("▌", theme::bold(theme::pal().accent))
        } else {
            Span::raw(" ")
        }];
        if let Some(n) = number {
            spans.push(n);
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(name, name_style));
        spans.push(Span::styled(" ".repeat(pad), fill));
        if self.size {
            let size = match e.kind {
                EntryKind::File => human_size(e.size),
                EntryKind::Drive(kind) => truncate(kind.label(), SIZE_W),
                EntryKind::Share => "share".to_string(),
                EntryKind::Pin => "pin".to_string(),
                EntryKind::Dir => String::new(),
                EntryKind::Machine(Some(true)) => "online".to_string(),
                EntryKind::Machine(Some(false)) => "offline".to_string(),
                EntryKind::Machine(None) => String::new(),
                EntryKind::Template(Template::Missing) => "missing".to_string(),
                EntryKind::Template(Template::NeedsUser) => "who? u".to_string(),
                EntryKind::Template(_) => String::new(),
            };
            spans.push(Span::styled(format!("{size:>SIZE_W$}"), meta_style));
        }
        if self.date {
            // Virtual entries have no dates; a share's remark goes there instead.
            let date = match e.kind {
                EntryKind::Share | EntryKind::Pin => truncate(&e.detail, DATE_W),
                // Machines: how they answered; templates: where they lead.
                EntryKind::Machine(_) => truncate(&e.detail, DATE_W),
                EntryKind::Template(_) => super::truncate_left(&template_path(e), DATE_W),
                _ if self.results && self.scored => format!("{:>5}", e.detail),
                _ => e.modified.map(fmt_minute).unwrap_or_default(),
            };
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

pub(super) fn fmt_second(t: SystemTime) -> String {
    DateTime::<Local>::from(t)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

pub(super) fn info_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<10}"), theme::dim()),
        Span::styled(value, theme::fg(theme::pal().fg)),
    ])
}

/// Where the preview put a picture, and (iTerm2) the picture the UI loop still has to
/// write there after the frame.
struct Drawn {
    rect: Rect,
    place: Option<Arc<ImagePreview>>,
}

/// A picture with a caption line (`1920×1080 · JPEG · 412.0 KB`). Returns where the
/// picture goes, so the UI loop can write it and erase it once it moves (`crate::preview`).
fn draw_image(f: &mut Frame, inner: Rect, app: &App, e: &Entry) -> Option<Drawn> {
    let area = app.image_area?;
    let caption_area = Rect {
        y: inner.bottom().saturating_sub(1),
        height: 1,
        ..inner
    };
    let state = app
        .cwd
        .join(&e.name)
        .ok()
        .and_then(|loc| app.image_of(&loc));
    let mut caption = human_size(e.size);
    let mut drawn = None;
    // Photo details for the row above the caption.
    let mut details: Option<String> = None;
    match state.map(|s| s.result.as_ref()) {
        Some(Some(Ok(p))) => {
            details = p.photo.as_ref().map(|i| i.line());
            caption = match &p.pdf {
                // `page 2 / 14 · A3 landscape · 420×297 mm (16.5×11.7 in) · 1.2 MB`
                Some(info) => format!("{} · {}", info.caption(), human_size(e.size)),
                None if p.thumbnail => format!("Windows thumbnail · {}", human_size(e.size)),
                None => format!(
                    "{}×{} · {} · {}",
                    p.width,
                    p.height,
                    p.format,
                    human_size(e.size)
                ),
            };
            let size = p.picture.size();
            // Overlays (help, pickers, which-key) cover the column: no picture under
            // them. An iTerm2 picture is written after the frame and would cover them.
            let covered = matches!(
                app.mode,
                Mode::Help
                    | Mode::Picker
                    | Mode::Credentials
                    | Mode::NetList
                    | Mode::SyncPreview
                    | Mode::OpenWith
                    | Mode::Rename
                    | Mode::Jobs
            ) || app.rename_panel.is_some() // its confirm prompt is a Prompt
                || app.which_key_visible();
            if !covered && size.width <= area.width && size.height <= area.height {
                let rect = Rect {
                    width: size.width,
                    height: size.height,
                    ..area
                };
                drawn = Some(Drawn { rect, place: None });
                match &p.picture {
                    // The UI loop writes it after the frame (crate::preview docs).
                    Picture::Iterm2 { .. } => {
                        drawn = Some(Drawn {
                            rect,
                            place: Some(Arc::clone(p)),
                        });
                    }
                    Picture::Cells(protocol) => f.render_widget(Image::new(protocol), rect),
                }
            }
        }
        Some(Some(Err(err))) => {
            let lines = vec![
                Line::default(),
                Line::from(Span::styled(
                    format!(
                        " ⚠ {}",
                        truncate(err, inner.width.saturating_sub(4) as usize)
                    ),
                    theme::fg(theme::pal().warn),
                )),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        Some(None) => {
            let what = match (PreviewKind::of(&e.name), state) {
                (Some(PreviewKind::Pdf), Some(s)) => format!("rendering page {}…", s.key.page + 1),
                _ => "rendering…".to_string(),
            };
            let line = Line::from(Span::styled(
                format!(" {} {what}", spinner(app)),
                theme::dim(),
            ));
            f.render_widget(Paragraph::new(vec![Line::default(), line]), inner);
        }
        // Waiting for the cursor to settle.
        None => {}
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {caption}"),
            theme::dim(),
        ))),
        caption_area,
    );
    // The row above the caption: in search results, where the file really is; otherwise
    // a photo's EXIF details.
    let above = Rect {
        y: caption_area.y.saturating_sub(1),
        ..caption_area
    };
    if app.search_state(&app.cwd).is_some() {
        if let Some(dir) = app.cwd.join(&e.name).ok().and_then(|t| t.parent()) {
            let w = (above.width as usize).saturating_sub(12);
            let dir = format!("{}\\", dir.display().trim_end_matches('\\'));
            f.render_widget(
                Paragraph::new(info_line("Found in", super::truncate_left(&dir, w))),
                above,
            );
        }
    } else if let Some(d) = details.filter(|d| !d.is_empty()) {
        let w = (above.width as usize).saturating_sub(2);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", truncate(&d, w)),
                theme::dim(),
            ))),
            above,
        );
    }
    drawn
}

/// A text file's first lines: line numbers (dim), the text cut at the edge, and what it
/// is on the last row (`UTF-8 · 42 lines`, `first 2 000 lines · zp to read more`).
fn draw_text(f: &mut Frame, area: Rect, tp: &crate::preview::text::TextPreview, scroll: usize) {
    let h = area.height as usize;
    let w = area.width as usize;
    if h == 0 || w < 8 {
        return;
    }
    let rows = h.saturating_sub(1);
    let start = scroll.min(tp.lines.len().saturating_sub(1));
    let last = (start + rows).min(tp.lines.len());
    let nw = last.max(1).to_string().len();
    let mut lines: Vec<Line> = tp.lines[start..last]
        .iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::styled(
                    format!(" {:>nw$} ", start + i + 1),
                    theme::fg(theme::pal().dimmed),
                ),
                Span::styled(
                    truncate(l, w.saturating_sub(nw + 2)),
                    theme::fg(theme::pal().fg),
                ),
            ])
        })
        .collect();
    if tp.lines.is_empty() {
        lines.push(Line::from(Span::styled(" (empty)", theme::dim())));
    }
    while lines.len() < rows {
        lines.push(Line::default());
    }
    let count = if tp.truncated {
        format!("first {} lines", thousands(tp.lines.len() as u64))
    } else {
        format!("{} lines", thousands(tp.lines.len() as u64))
    };
    let more = if last < tp.lines.len() || tp.truncated {
        "  ·  zp: full screen"
    } else {
        ""
    };
    lines.push(Line::from(Span::styled(
        truncate(&format!(" {} · {count}{more}", tp.encoding), w),
        theme::dim(),
    )));
    f.render_widget(Paragraph::new(lines), area);
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
    // In search results the entry name is a relative path: show where it really is.
    let target = app.cwd.join(&e.name).ok();
    let (dir, name) = match target.as_ref().and_then(|t| Some((t.parent()?, t.name()))) {
        Some((dir, name)) => (dir.display(), name),
        None => (app.cwd.display(), e.name.clone()),
    };
    let dir = format!("{}\\", dir.trim_end_matches('\\'));
    let in_results = app.search_state(&app.cwd).is_some();
    lines.push(info_line(
        if in_results { "Found in" } else { "Path" },
        super::truncate_left(&dir, w),
    ));
    lines.push(info_line("", truncate(&name, w)));
    if in_results {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(" gr", theme::bold(theme::pal().fg)),
            Span::styled(" go to this folder", theme::dim()),
        ]));
    }
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
pub(super) fn scrolled_offset(cursor: usize, offset: usize, len: usize, height: usize) -> usize {
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
