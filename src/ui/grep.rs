//! The preview of a content-search result (`S`, CLAUDE.md section 12): the file's
//! matching lines with context, matches highlighted, scrolled to the match `]m` / `[m`
//! is on. Only the visible rows are turned into spans.

use super::columns::info_line;
use super::theme::{self, ACCENT, DIMMED, ERR, FG, ON_ACCENT};
use super::{spinner, thousands, truncate, truncate_left};
use crate::app::App;
use crate::fs::Entry;
use crate::search::content::{MatchPreview, PreviewLine};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const TAB: &str = "    ";
/// Characters kept left of a match when a long line has to be shifted to show it.
const LEAD: usize = 8;
/// Rows under the lines: blank, matches, found in, folder.
const FOOTER: u16 = 4;

/// One row of the listing: a line of the file, or the `┈` marking skipped lines.
enum Row<'a> {
    Gap,
    Line(&'a PreviewLine, Option<usize>),
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, e: &Entry) {
    let target = app.cwd.join(&e.name).ok();
    let held = target.as_ref().and_then(|t| app.grep_preview_of(t));
    let footer_h = FOOTER.min(area.height);
    let body = Rect {
        height: area.height - footer_h,
        ..area
    };
    let footer = Rect {
        y: area.y + body.height,
        height: footer_h,
        ..area
    };
    let mut summary = format!("{} lines", thousands(e.detail.parse().unwrap_or(0)));
    match held.and_then(|p| p.result.as_ref().map(|r| (p, r))) {
        None => {
            let msg = format!(" {} reading the file…", spinner(app));
            f.render_widget(Paragraph::new(Span::styled(msg, theme::dim())), body);
        }
        Some((_, Err(err))) => {
            let msg = format!(" ⚠ can't read it: {err}");
            f.render_widget(Paragraph::new(Span::styled(msg, theme::fg(ERR))), body);
        }
        Some((_, Ok(p))) if p.matches.is_empty() => {
            let why = if p.binary {
                " binary now: it changed since the search"
            } else {
                " no matches now: the file changed since the search"
            };
            f.render_widget(Paragraph::new(Span::styled(why, theme::dim())), body);
        }
        Some((held, Ok(p))) => {
            let current = held.current.min(p.matches.len() - 1);
            draw_lines(f, body, p, current);
            let more = if p.truncated { "+" } else { "" };
            summary = format!(
                "{summary} · match {} / {}{more}",
                current + 1,
                thousands(p.matches.len() as u64)
            );
        }
    }
    draw_footer(f, footer, app, e, summary);
}

fn draw_lines(f: &mut Frame, area: Rect, p: &MatchPreview, current: usize) {
    if area.height == 0 {
        return;
    }
    let mut rows = Vec::with_capacity(p.lines.len() + 8);
    let mut current_row = 0;
    let mut match_no = 0;
    for line in &p.lines {
        if line.gap_before {
            rows.push(Row::Gap);
        }
        let idx = (!line.hits.is_empty()).then(|| {
            match_no += 1;
            match_no - 1
        });
        if idx == Some(current) {
            current_row = rows.len();
        }
        rows.push(Row::Line(line, idx));
    }
    let height = area.height as usize;
    // The current match sits a third of the way down, like vim's search with scrolloff.
    let offset = current_row
        .saturating_sub(height / 3)
        .min(rows.len().saturating_sub(height));
    let num_w = p
        .lines
        .last()
        .map_or(1, |l| l.number.to_string().len())
        .max(3);
    let text_w = (area.width as usize).saturating_sub(num_w + 3);
    let lines: Vec<Line> = rows
        .iter()
        .skip(offset)
        .take(height)
        .map(|row| match row {
            Row::Gap => Line::from(Span::styled(format!(" {:>num_w$}", "┈"), theme::fg(DIMMED))),
            Row::Line(l, idx) => {
                let is_current = *idx == Some(current);
                let (mark, num_style) = match (is_current, idx.is_some()) {
                    (true, _) => ("›", theme::bold(ACCENT)),
                    (false, true) => (" ", theme::fg(ACCENT)),
                    (false, false) => (" ", theme::fg(DIMMED)),
                };
                let mut spans = vec![
                    Span::styled(mark, theme::bold(ACCENT)),
                    Span::styled(format!("{:>num_w$} ", l.number), num_style),
                ];
                spans.extend(text_spans(l, text_w, is_current));
                Line::from(spans)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// The line's text cut to `w` cells, tabs expanded, matches highlighted. A match beyond
/// the right edge shifts the line left (with a `…`) so it stays in view.
fn text_spans(l: &PreviewLine, w: usize, is_current: bool) -> Vec<Span<'static>> {
    let base = if l.hits.is_empty() {
        theme::dim()
    } else {
        theme::fg(FG)
    };
    let hit_style = if is_current {
        Style::new()
            .bg(ACCENT)
            .fg(ON_ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        theme::bold(ACCENT).add_modifier(Modifier::UNDERLINED)
    };
    // (char, highlighted), tabs expanded.
    let mut cells: Vec<(char, bool)> = Vec::with_capacity(l.text.len());
    let mut first_hit = None;
    for (i, c) in l.text.char_indices() {
        let hl = l.hits.iter().any(|&(s, e)| i >= s && i < e);
        if hl && first_hit.is_none() {
            first_hit = Some(cells.len());
        }
        if c == '\t' {
            cells.extend(TAB.chars().map(|t| (t, hl)));
        } else if !c.is_control() {
            cells.push((c, hl));
        }
    }
    let shift = match first_hit {
        Some(at) if at + LEAD >= w => at - LEAD.min(at),
        _ => 0,
    };
    let mut spans = Vec::new();
    let mut avail = w;
    if shift > 0 && avail > 0 {
        spans.push(Span::styled("…", theme::fg(DIMMED)));
        avail -= 1;
    }
    let mut run = String::new();
    let mut run_hl = false;
    for &(c, hl) in cells.iter().skip(shift).take(avail) {
        if hl != run_hl && !run.is_empty() {
            let style = if run_hl { hit_style } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_hl = hl;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hl { hit_style } else { base }));
    }
    spans
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App, e: &Entry, summary: String) {
    let w = (area.width as usize).saturating_sub(12);
    let dir = app
        .cwd
        .join(&e.name)
        .ok()
        .and_then(|t| t.parent())
        .map_or_else(|| app.cwd.display(), |d| d.display());
    let dir = format!("{}\\", dir.trim_end_matches('\\'));
    let lines = vec![
        Line::default(),
        info_line("Matches", truncate(&summary, w)),
        info_line("Found in", truncate_left(&dir, w)),
        Line::from(vec![
            Span::styled(" ]m [m", theme::bold(FG)),
            Span::styled(" step matches   ", theme::dim()),
            Span::styled("gr", theme::bold(FG)),
            Span::styled(" go to this folder", theme::dim()),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), area);
}
