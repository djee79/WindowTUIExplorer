//! The picker (docs/screens/03-launcher.txt): prompt box, results with the tag bar, and
//! a preview panel. Only the visible rows are built.

use super::columns::{gutter_cell, gutter_width, info_line, panel, scrolled_offset};
use super::theme::{self, ACCENT, BORDER, DIM, ERR, FG, NET, OK, SEL_BG};
use super::{spinner, truncate, truncate_left, width};
use crate::app::App;
use crate::fs::EntryKind;
use crate::picker::{PickItem, PickerMode};
use crate::pins::PinKind;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::collections::HashSet;

/// `[backup] ` is the widest kind label.
const KIND_W: usize = 9;

pub fn draw(f: &mut Frame, body: Rect, app: &mut App) {
    f.render_widget(Clear, body);
    f.render_widget(
        ratatui::widgets::Block::new().style(Style::new().bg(theme::BG).fg(FG)),
        body,
    );
    let [left, right] =
        Layout::horizontal([Constraint::Fill(77), Constraint::Fill(82)]).areas(body);
    let [prompt, results] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(left);
    draw_prompt(f, prompt, app);
    draw_results(f, results, app);
    draw_preview(f, right, app);
}

fn draw_prompt(f: &mut Frame, area: Rect, app: &App) {
    let Some(p) = app.picker.as_ref() else { return };
    let block = panel(String::new(), true);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = p.editor.text();
    let cur = p.editor.cursor();
    let (before, after): (String, String) = (
        text.chars().take(cur).collect(),
        text.chars().skip(cur).collect(),
    );
    let count = format!(
        "{}{} / {} ",
        if p.running {
            format!("{} ", spinner(app))
        } else {
            String::new()
        },
        p.matched,
        p.total()
    );
    let mut spans = vec![
        Span::styled(" > ", theme::bold(ACCENT)),
        Span::styled(before.clone(), theme::fg(FG)),
    ];
    if p.mode == PickerMode::Insert {
        spans.push(Span::styled("▏", theme::fg(ACCENT)));
    }
    spans.push(Span::styled(after.clone(), theme::fg(FG)));
    let used = 3 + width(&before) + width(&after) + 1;
    let pad = (inner.width as usize).saturating_sub(used + width(&count));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(count, theme::dim()));
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_results(f: &mut Frame, area: Rect, app: &mut App) {
    let block = panel(
        app.picker
            .as_ref()
            .map_or(String::new(), |p| p.title.to_string()),
        true,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (number, relative) = (app.number, app.relative_number);
    let Some(p) = app.picker.as_mut() else { return };
    let w = inner.width as usize;

    let mut lines: Vec<Line> = vec![top_bar(p, w), separator(w)];
    // Filter help at the bottom when there's room for it.
    let help = help_lines(p, w);
    let rows_h = (inner.height as usize).saturating_sub(lines.len() + help.len());
    p.page = rows_h;
    p.offset = scrolled_offset(p.cursor, p.offset, p.view.len(), rows_h);
    let normal = p.mode == PickerMode::Normal;
    let gw = if normal {
        gutter_width(p.view.len(), number, relative)
    } else {
        0
    };
    let visible: Vec<_> = p.view.iter().skip(p.offset).take(rows_h).cloned().collect();
    for (i, item) in visible.iter().enumerate() {
        let row = p.offset + i;
        let hits: HashSet<u32> = if p.query.text.is_empty() {
            HashSet::new()
        } else {
            p.match_indices(item).into_iter().collect()
        };
        let gutter = normal.then(|| gutter_cell(row, p.cursor, gw, number, relative));
        lines.push(result_line(item, &hits, w, row == p.cursor, gutter));
    }
    if p.view.is_empty() && !p.running {
        lines.push(Line::from(Span::styled(
            if p.total() == 0 {
                "  no pins yet: m{key} in a folder, or M on an item"
            } else {
                "  no match"
            },
            theme::dim(),
        )));
    }
    while lines.len() < 2 + rows_h {
        lines.push(Line::default());
    }
    lines.extend(help);
    f.render_widget(Paragraph::new(lines), inner);
}

/// The tag bar (`#aveva 23   #acme 40 …`), or the completion candidates while a filter
/// word is being typed.
fn top_bar(p: &crate::picker::Picker, w: usize) -> Line<'static> {
    if let Some(c) = &p.completion {
        let mut spans = vec![Span::styled(" Tab ", theme::bold(ACCENT))];
        for (name, n) in c.candidates.iter().take(8) {
            spans.push(Span::styled(name.clone(), theme::fg(FG)));
            spans.push(Span::styled(format!(" {n}   "), theme::dim()));
        }
        return Line::from(spans);
    }
    let active: Vec<&String> = p.query.tags.iter().flatten().collect();
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, (tag, n)) in p.tag_bar.iter().enumerate() {
        let text = format!("#{tag} {n}   ");
        let more = format!("+{} more", p.tag_bar.len() - i);
        if used + width(&text) + width(&more) > w && i + 1 < p.tag_bar.len() {
            spans.push(Span::styled(more, theme::dim()));
            break;
        }
        used += width(&text);
        let on = active.iter().any(|a| tag.starts_with(a.as_str()));
        let style = if on {
            theme::bold(ACCENT).add_modifier(Modifier::UNDERLINED)
        } else {
            theme::fg(NET)
        };
        spans.push(Span::styled(format!("#{tag}"), style));
        spans.push(Span::styled(format!(" {n}   "), theme::dim()));
    }
    if p.tag_bar.is_empty() {
        spans.push(Span::styled(
            "no tags yet (^t on a pin adds some)",
            theme::dim(),
        ));
    }
    Line::from(spans)
}

fn separator(w: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {}", "─".repeat(w.saturating_sub(2))),
        theme::fg(BORDER),
    ))
}

fn help_lines(p: &crate::picker::Picker, w: usize) -> Vec<Line<'static>> {
    let mut out = vec![separator(w)];
    if let Some(problem) = p.query.problems.first() {
        out.push(Line::from(Span::styled(
            format!(" {problem}"),
            theme::fg(ERR),
        )));
    }
    for text in [
        " filters: #tag and  #a,b or  !#tag not",
        "          is:folder is:web is:unc is:unreachable  on:<server>",
        " ranked: name prefix › tag prefix › substring › fuzzy",
    ] {
        out.push(Line::from(Span::styled(text, theme::dim())));
    }
    out
}

fn kind_color(kind: PinKind) -> ratatui::style::Color {
    match kind {
        PinKind::Folder => theme::DIR,
        PinKind::File => FG,
        PinKind::App => OK,
        PinKind::Web => NET,
        PinKind::Sync | PinKind::Backup => theme::WARN,
    }
}

/// `[folder] HIT (server)          'h          #aveva #servers`
fn result_line(
    item: &PickItem,
    hits: &HashSet<u32>,
    w: usize,
    is_cursor: bool,
    gutter: Option<Span<'static>>,
) -> Line<'static> {
    let base = if is_cursor {
        Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut spans = vec![Span::styled(" ", base)];
    let mut used = 1;
    if let Some(g) = gutter {
        used += width(&g.content) + 1;
        spans.push(g.patch_style(base));
        spans.push(Span::styled(" ", base));
    }
    let kind = format!("[{}]", item.kind.label());
    spans.push(Span::styled(
        format!("{kind:<KIND_W$}"),
        base.fg(kind_color(item.kind)),
    ));
    used += KIND_W;
    let key = item.key.map(|c| format!("'{c}")).unwrap_or_default();
    let tags = item
        .tags
        .iter()
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ");
    let name_w = (w.saturating_sub(used + 4 + width(&tags) + 1)).clamp(10, 40);
    let name = truncate(&item.name, name_w);
    // Highlight the matched characters of the name (they come first in the haystack).
    for (i, c) in name.chars().enumerate() {
        let style = if hits.contains(&(i as u32)) {
            base.fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            base.fg(FG)
        };
        spans.push(Span::styled(c.to_string(), style));
    }
    spans.push(Span::styled(
        " ".repeat(name_w.saturating_sub(width(&name)) + 1),
        base,
    ));
    spans.push(Span::styled(format!("{key:<3}"), base.fg(ACCENT)));
    used += name_w + 1 + 3;
    let tags = truncate(&tags, w.saturating_sub(used + 2));
    let pad = w.saturating_sub(used + width(&tags) + 1);
    spans.push(Span::styled(" ".repeat(pad), base));
    spans.push(Span::styled(tags, base.fg(NET)));
    spans.push(Span::styled(" ", base));
    Line::from(spans)
}

fn draw_preview(f: &mut Frame, area: Rect, app: &App) {
    let item = app.picker.as_ref().and_then(|p| p.selected()).cloned();
    let title = item.as_ref().map(|i| i.name.clone()).unwrap_or_default();
    let block = panel(title, false);
    let block = match &item {
        Some(i) => block.title(
            Line::from(Span::styled(
                format!(" {} pin ", i.kind.label()),
                theme::dim(),
            ))
            .right_aligned(),
        ),
        None => block,
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(item) = item else { return };
    let Some(pin) = app.pins.pins.get(item.id) else {
        return;
    };
    let vw = (inner.width as usize).saturating_sub(12);
    let extra = |k: &str| {
        pin.extra
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };

    let mut lines = vec![Line::default()];
    match pin.kind {
        PinKind::Folder | PinKind::File => {
            lines.push(info_line("Path", truncate_left(&pin.detail(), vw)));
        }
        PinKind::App => {
            lines.push(info_line("Command", truncate_left(&pin.detail(), vw)));
            if let Some(a) = &pin.args {
                lines.push(info_line("Args", truncate(a, vw)));
            }
            if let Some(d) = &pin.dir {
                lines.push(info_line("Dir", truncate_left(d, vw)));
            }
        }
        PinKind::Web => {
            lines.push(info_line("URL", truncate(&pin.detail(), vw)));
            lines.push(info_line(
                "Browser",
                pin.browser.clone().unwrap_or_else(|| "edge".into()),
            ));
        }
        PinKind::Sync | PinKind::Backup => {
            lines.push(info_line("Source", truncate_left(&pin.detail(), vw)));
            let dest = extra("dest")
                .or_else(|| extra("dest_dir"))
                .unwrap_or_default();
            lines.push(info_line("Dest", truncate_left(&dest, vw)));
            if let Some(m) = extra("mode") {
                lines.push(info_line("Mode", m));
            }
        }
    }
    lines.push(info_line(
        "Key",
        item.key.map_or("none".to_string(), |c| format!("'{c}")),
    ));
    let tags = item
        .tags
        .iter()
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(Line::from(vec![
        Span::styled(format!(" {:<10}", "Tags"), theme::dim()),
        Span::styled(
            if tags.is_empty() {
                "none".to_string()
            } else {
                tags
            },
            theme::fg(NET),
        ),
        Span::styled("   ^t edit tags", theme::dim()),
    ]));
    if let Some(server) = &item.server {
        lines.push(info_line("Server", server.clone()));
    }
    lines.extend(reach_line(app, pin));

    // A folder pin previews its listing (the same cache as the columns).
    if pin.kind == PinKind::Folder {
        lines.push(Line::default());
        lines.extend(listing_lines(app, pin, inner.height as usize, vw + 10));
    }
    let h = inner.height as usize;
    lines.truncate(h.saturating_sub(2));
    while lines.len() < h.saturating_sub(1) {
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![
        Span::styled(" Enter", theme::bold(FG)),
        Span::styled(" open   ", theme::dim()),
        Span::styled("^e", theme::bold(FG)),
        Span::styled(" edit   ", theme::dim()),
        Span::styled("^t", theme::bold(FG)),
        Span::styled(" tags   ", theme::dim()),
        Span::styled("^x", theme::bold(FG)),
        Span::styled(" delete", theme::dim()),
    ]));
    f.render_widget(Paragraph::new(lines), inner);
}

fn reach_line(app: &App, pin: &crate::pins::Pin) -> Option<Line<'static>> {
    let target = pin.target()?;
    let r = app.reach_of(&target)?;
    let (text, color) = if r.is_reachable() {
        ("● reachable", OK)
    } else if r.is_unreachable() {
        ("× unreachable", ERR)
    } else {
        return None;
    };
    Some(Line::from(vec![
        Span::styled(format!(" {:<10}", "Status"), theme::dim()),
        Span::styled(text, theme::fg(color)),
    ]))
}

fn listing_lines(app: &App, pin: &crate::pins::Pin, h: usize, w: usize) -> Vec<Line<'static>> {
    let Some(target) = pin.target() else {
        return Vec::new();
    };
    if app.reach_of(&target).is_some_and(|r| r.is_unreachable()) {
        return vec![Line::from(Span::styled(
            " not listed: the last probe failed",
            theme::dim(),
        ))];
    }
    let Some(state) = app.dir(&target) else {
        return vec![Line::from(Span::styled(" …", theme::dim()))];
    };
    if let Some(e) = &state.error {
        return vec![Line::from(Span::styled(format!(" ⚠ {e}"), theme::fg(ERR)))];
    }
    let Some(listing) = &state.listing else {
        return vec![Line::from(Span::styled(
            format!(" {} listing…", spinner(app)),
            theme::dim(),
        ))];
    };
    let mut out: Vec<Line> = listing
        .entries
        .iter()
        .filter(|e| app.show_hidden || !e.is_hidden())
        .take(h)
        .map(|e| {
            let (suffix, color) = match e.kind {
                EntryKind::File => ("", FG),
                _ => ("\\", theme::DIR),
            };
            Line::from(Span::styled(
                format!(" {}", truncate(&format!("{}{suffix}", e.name), w)),
                theme::fg(color),
            ))
        })
        .collect();
    if out.is_empty() {
        out.push(Line::from(Span::styled(" empty", theme::fg(DIM))));
    }
    out
}
