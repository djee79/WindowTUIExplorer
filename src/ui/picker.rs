//! The picker (docs/screens/03-launcher.txt): prompt box, results with the tag bar, and
//! a preview panel. Only the visible rows are built.

use super::columns::{gutter_cell, gutter_width, info_line, panel, scrolled_offset};
use super::theme::{self, ACCENT, BORDER, DIM, ERR, FG, NET, OK, SEL_BG};
use super::{spinner, truncate, truncate_left, width};
use crate::app::App;
use crate::fs::EntryKind;
use crate::picker::{ItemKind, PickItem, PickerMode, Source};
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
    // A walk feeding the picker counts what it scanned (`matched / scanned`).
    let of = if p.walking || p.scanned > 0 {
        format!("{} scanned", p.scanned.max(p.total() as u64))
    } else {
        p.total().to_string()
    };
    let count = format!(
        "{}{} / {of} ",
        if p.running || p.walking {
            format!("{} ", spinner(app))
        } else {
            String::new()
        },
        p.matched,
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
    // Machines' dots are live (the port-445 probes land after the rows were made).
    let online = app.machine_online();
    let note = machine_note(app);
    let Some(p) = app.picker.as_mut() else { return };
    let w = inner.width as usize;

    let mut lines: Vec<Line> = vec![top_bar(p, w), separator(w)];
    // Filter help at the bottom when there's room for it.
    let mut help = help_lines(p, w);
    if p.source == Source::Machines
        && let Some(n) = note
    {
        help.insert(1, n);
    }
    let rows_h = (inner.height as usize).saturating_sub(lines.len() + help.len());
    p.page = rows_h;
    p.offset = scrolled_offset(p.cursor, p.offset, p.view.len(), rows_h);
    let normal = p.mode == PickerMode::Normal;
    let gw = if normal {
        gutter_width(p.view.len(), number, relative)
    } else {
        0
    };
    // Section headers (the launcher's browse view) take a line each: scroll further
    // until the cursor's line fits under them.
    let headers = p.headers.clone();
    let header_at = |row: usize| {
        headers
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, t)| t.clone())
    };
    let lines_to = |from: usize, to: usize| {
        (from..=to)
            .map(|r| 1 + usize::from(header_at(r).is_some()))
            .sum::<usize>()
    };
    while p.offset < p.cursor && lines_to(p.offset, p.cursor) > rows_h {
        p.offset += 1;
    }
    let mut budget = rows_h;
    let mut visible = Vec::new();
    for (row, item) in p.view.iter().enumerate().skip(p.offset) {
        let need = 1 + usize::from(header_at(row).is_some());
        if need > budget {
            break;
        }
        budget -= need;
        visible.push((row, item.clone()));
    }
    for (row, item) in visible {
        if let Some(title) = header_at(row) {
            lines.push(Line::from(vec![
                Span::styled(format!(" {title} "), theme::bold(ACCENT)),
                Span::styled(
                    "─".repeat(w.saturating_sub(width(&title) + 3)),
                    theme::fg(theme::BORDER),
                ),
            ]));
        }
        let item = &item;
        let hits: HashSet<u32> = if p.query.text.is_empty() {
            HashSet::new()
        } else {
            p.match_indices(item).into_iter().collect()
        };
        let gutter = normal.then(|| gutter_cell(row, p.cursor, gw, number, relative));
        let chosen = p.is_chosen(item.id);
        let live = item
            .kind
            .is_machine()
            .then(|| item.server.as_ref().and_then(|s| online.get(s)).copied())
            .flatten();
        let item = match live {
            Some(up) if item.reachable != Some(up) => std::sync::Arc::new(PickItem {
                reachable: Some(up),
                ..(**item).clone()
            }),
            _ => item.clone(),
        };
        lines.push(result_line(
            &item,
            &hits,
            w,
            row == p.cursor,
            chosen,
            gutter,
        ));
    }
    if p.view.is_empty() && !p.running {
        let empty = match p.source {
            _ if p.walking => "  searching…",
            Source::Grep if p.editor.text().trim().is_empty() => {
                "  type what to look for in the files' contents"
            }
            Source::Files | Source::Grep => "  nothing found",
            Source::Dirs if p.total() == 0 => "  no folders visited yet",
            Source::Machines if p.total() == 0 => {
                r"  no machines yet: open one (\\HOST\C$), or set machines.inventory_csv"
            }
            Source::Pins | Source::Launcher if p.total() == 0 => {
                "  no pins yet: m{key} in a folder, or M on an item"
            }
            _ => "  no match",
        };
        lines.push(Line::from(Span::styled(empty, theme::dim())));
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
    if p.source == Source::Tags {
        let chosen = match p.chosen.len() {
            0 => "none chosen".to_string(),
            n => format!("{n} chosen"),
        };
        return Line::from(vec![
            Span::styled(" Tab", theme::bold(FG)),
            Span::styled(" choose   ", theme::dim()),
            Span::styled("Enter", theme::bold(FG)),
            Span::styled(" pins with all of them   ", theme::dim()),
            Span::styled(chosen, theme::fg(ACCENT)),
        ]);
    }
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
    if p.source == Source::Tags {
        out.push(Line::from(Span::styled(
            " ^e renames, ^x deletes a tag across all pins (asks first)",
            theme::dim(),
        )));
        return out;
    }
    if matches!(
        p.source,
        Source::Files | Source::Dirs | Source::Grep | Source::Commands | Source::Keymaps
    ) {
        if let Some(note) = &p.note {
            out.push(Line::from(Span::styled(format!(" {note}"), theme::dim())));
        }
        let keys = if matches!(p.source, Source::Commands | Source::Keymaps) {
            " Enter runs it · type to search · Esc then q closes"
        } else {
            " Enter open · ^o open with · ^v reveal · ^y copy path · ^a pin · Tab choose"
        };
        out.push(Line::from(Span::styled(keys, theme::dim())));
        return out;
    }
    if let Some(problem) = p.query.problems.first() {
        out.push(Line::from(Span::styled(
            format!(" {problem}"),
            theme::fg(ERR),
        )));
    }
    let second = if p.source == Source::Machines {
        "          is:online is:offline  (type a user to find their machine)"
    } else {
        "          is:folder is:web is:unc is:unreachable  on:<server>"
    };
    let third = if p.source == Source::Launcher {
        " ^s Start Menu apps · ^a pin (a URL, or the Start Menu app) · no match: Enter runs it"
    } else {
        " ranked: name prefix › tag prefix › substring › fuzzy"
    };
    for text in [" filters: #tag and  #a,b or  !#tag not", second, third] {
        out.push(Line::from(Span::styled(text, theme::dim())));
    }
    out
}

fn kind_color(kind: ItemKind) -> ratatui::style::Color {
    match kind {
        ItemKind::Pin(PinKind::Folder) => theme::DIR,
        ItemKind::Pin(PinKind::File) => FG,
        ItemKind::Pin(PinKind::App) => OK,
        ItemKind::Pin(PinKind::Web) | ItemKind::Tag => NET,
        ItemKind::Pin(PinKind::Sync | PinKind::Backup) => theme::WARN,
        ItemKind::Pin(PinKind::Machine) | ItemKind::Machine => ERR,
        ItemKind::View => ACCENT,
        ItemKind::StartMenu => OK,
        ItemKind::Action | ItemKind::Command => ACCENT,
        ItemKind::File => FG,
        ItemKind::Dir => theme::DIR,
        ItemKind::Binding => OK,
    }
}

/// The reachability dot (pins with a place), or the chosen mark (tag picker).
fn lead_mark(item: &PickItem, chosen: bool) -> Span<'static> {
    match (item.kind, chosen, item.reachable) {
        (ItemKind::Tag, true, _) => Span::styled("✓", theme::bold(ACCENT)),
        (ItemKind::Tag, false, _) => Span::styled("·", theme::dim()),
        (_, _, Some(true)) => Span::styled("●", theme::fg(OK)),
        (_, _, Some(false)) => Span::styled("×", theme::fg(ERR)),
        (ItemKind::Pin(PinKind::Folder | PinKind::File), _, None) => {
            Span::styled("○", theme::dim())
        }
        _ => Span::raw(" "),
    }
}

/// `[folder] HIT (server)          'h          #aveva #servers`
fn result_line(
    item: &PickItem,
    hits: &HashSet<u32>,
    w: usize,
    is_cursor: bool,
    chosen: bool,
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
    spans.push(lead_mark(item, chosen).patch_style(base));
    spans.push(Span::styled(" ", base));
    used += 2;
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
        Some(i) => {
            let what = match i.kind {
                ItemKind::Pin(k) => format!(" {} pin ", k.label()),
                other => format!(" {} ", other.label()),
            };
            block.title(Line::from(Span::styled(what, theme::dim())).right_aligned())
        }
        None => block,
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(item) = item else { return };
    match item.kind {
        ItemKind::View => return draw_view_preview(f, inner, app, &item),
        ItemKind::Tag => return draw_tag_preview(f, inner, app, &item),
        ItemKind::Machine => return draw_machine_preview(f, inner, app, &item),
        ItemKind::File | ItemKind::Dir => {
            let vw = (inner.width as usize).saturating_sub(12);
            let path = app
                .picker_paths
                .get(item.id)
                .map_or_else(|| item.name.clone(), |p| p.path.clone());
            let mut lines = vec![Line::default(), info_line("Path", truncate_left(&path, vw))];
            if !item.detail.is_empty() {
                lines.push(info_line("Found", item.detail.clone()));
            }
            lines.push(Line::default());
            for hint in [
                if item.kind == ItemKind::Dir {
                    " Enter goes there"
                } else {
                    " Enter opens it (its folder, then its first opener)"
                },
                " ^o open with…  ^v reveal in Explorer  ^y copy path",
                " ^a pin it  Tab choose several (Enter yanks them)",
            ] {
                lines.push(Line::from(Span::styled(hint, theme::dim())));
            }
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        ItemKind::Binding | ItemKind::Command => {
            let lines = vec![
                Line::default(),
                Line::from(Span::styled(format!(" {}", item.name), theme::bold(ACCENT))),
                Line::default(),
                info_line(
                    if item.kind == ItemKind::Binding {
                        "Keys"
                    } else {
                        "Type"
                    },
                    item.detail.clone(),
                ),
                Line::default(),
                Line::from(Span::styled(" Enter runs it", theme::dim())),
            ];
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        ItemKind::Action => {
            let lines = vec![
                Line::default(),
                Line::from(Span::styled(format!(" {}", item.name), theme::bold(ACCENT))),
                Line::default(),
                info_line("Also", item.detail.clone()),
                Line::default(),
                Line::from(Span::styled(" Enter does it", theme::dim())),
            ];
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        ItemKind::StartMenu => {
            let vw = (inner.width as usize).saturating_sub(12);
            let lines = vec![
                Line::default(),
                info_line("App", item.name.clone()),
                info_line("Shortcut", truncate_left(&item.detail, vw)),
                Line::default(),
                Line::from(Span::styled(
                    " Enter runs it · ^a pins it · ^s hides the Start Menu",
                    theme::dim(),
                )),
            ];
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        ItemKind::Pin(_) => {}
    }
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
            if pin.kind == PinKind::Backup {
                let keep = match pin.extra.get("keep").and_then(|v| v.as_integer()) {
                    None | Some(0) => "all zips".to_string(),
                    Some(n) => format!("newest {n}"),
                };
                lines.push(info_line("Keep", keep));
                lines.push(info_line(
                    "Engine",
                    extra("engine").unwrap_or_else(|| "builtin".into()),
                ));
            }
            if matches!(pin.kind, PinKind::Sync | PinKind::Backup) {
                let last = app.last_run_line(pin).unwrap_or_else(|| "never".into());
                lines.push(info_line("Last run", truncate(&last, vw)));
            }
        }
        PinKind::Machine => {
            lines.push(info_line("Host", extra("host").unwrap_or_default()));
            lines.extend(machine_lines(app, &item, vw));
            lines.push(info_line(
                "Login",
                extra("auth").unwrap_or_else(|| "admin".to_string()),
            ));
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

/// Fills `lines` to the panel height and ends with a key hint line.
fn with_hints(
    mut lines: Vec<Line<'static>>,
    h: usize,
    hints: &[(&str, &str)],
) -> Vec<Line<'static>> {
    lines.truncate(h.saturating_sub(2));
    while lines.len() < h.saturating_sub(1) {
        lines.push(Line::default());
    }
    let mut spans = Vec::new();
    for (k, what) in hints {
        spans.push(Span::styled(format!(" {k}"), theme::bold(FG)));
        spans.push(Span::styled(format!(" {what}  "), theme::dim()));
    }
    lines.push(Line::from(spans));
    lines
}

fn draw_view_preview(f: &mut Frame, inner: Rect, app: &App, item: &PickItem) {
    let mut lines = vec![
        Line::default(),
        info_line("Query", item.detail.clone()),
        info_line(
            "Key",
            item.key.map_or("none".to_string(), |c| format!("'{c}")),
        ),
        Line::default(),
        Line::from(Span::styled(
            " A saved view: Enter opens the pin picker with this query.",
            theme::dim(),
        )),
    ];
    // What it currently matches (filters only; the fuzzy part needs the matcher).
    let q = crate::picker::query::PinQuery::parse(&item.detail);
    let n = app
        .pins
        .pins
        .iter()
        .filter(|p| {
            q.matches(&crate::picker::query::Facts {
                kind: Some(p.kind),
                tags: &p.tags,
                ..Default::default()
            })
        })
        .count();
    if !q.tags.is_empty() || !q.not_tags.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" Its tags match {n} pins now."),
            theme::dim(),
        )));
    }
    let lines = with_hints(
        lines,
        inner.height as usize,
        &[("Enter", "open"), ("^e", "rename / key"), ("^x", "delete")],
    );
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_tag_preview(f: &mut Frame, inner: Rect, app: &App, item: &PickItem) {
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(format!(" #{}", item.name), theme::bold(NET)),
            Span::styled(format!("  {}", item.detail), theme::dim()),
        ]),
        Line::default(),
    ];
    for p in app.pins.pins.iter().filter(|p| p.tags.contains(&item.name)) {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<9}", format!("[{}]", p.kind.label())),
                theme::dim(),
            ),
            Span::styled(p.name.clone(), theme::fg(FG)),
        ]));
    }
    let lines = with_hints(
        lines,
        inner.height as usize,
        &[
            ("Tab", "choose"),
            ("Enter", "pins with all chosen"),
            ("^e", "rename"),
            ("^x", "delete"),
        ],
    );
    f.render_widget(Paragraph::new(lines), inner);
}

/// A machine that isn't pinned: what the sources say, and the live probe.
fn draw_machine_preview(f: &mut Frame, inner: Rect, app: &App, item: &PickItem) {
    let mut lines = vec![Line::default()];
    lines.extend(machine_lines(
        app,
        item,
        (inner.width as usize).saturating_sub(12),
    ));
    let h = inner.height as usize;
    lines.truncate(h.saturating_sub(2));
    while lines.len() < h.saturating_sub(1) {
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![
        Span::styled(" Enter", theme::bold(FG)),
        Span::styled(" open   ", theme::dim()),
        Span::styled("^a", theme::bold(FG)),
        Span::styled(" pin it", theme::dim()),
    ]));
    f.render_widget(Paragraph::new(lines), inner);
}

/// Host, user, where it was found, details, and the port-445 status.
fn machine_lines(app: &App, item: &PickItem, vw: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let Some(host) = app.picked_host(item) else {
        return out;
    };
    let record = app.machine_record(item);
    if item.kind == ItemKind::Machine {
        out.push(info_line("Host", host.clone()));
    }
    let user = record
        .as_ref()
        .and_then(|r| r.user.clone())
        .or_else(|| app.machine_user(&host));
    if let Some(u) = user {
        out.push(info_line("User", u));
    }
    if let Some(r) = &record {
        out.push(info_line("Found in", r.source.to_string()));
        if !r.details.is_empty() {
            out.push(info_line("Details", truncate(&r.details.join(" · "), vw)));
        }
    }
    let (online, status) = app.machine_status(&host);
    let (mark, color) = match online {
        Some(true) => ("● ", OK),
        Some(false) => ("× ", ERR),
        None => ("○ ", DIM),
    };
    out.push(Line::from(vec![
        Span::styled(format!(" {:<10}", "Status"), theme::dim()),
        Span::styled(format!("{mark}{}", truncate(&status, vw)), theme::fg(color)),
    ]));
    out
}

/// `⠋ loading the inventory…` or the first source error, under the separator.
fn machine_note(app: &App) -> Option<Line<'static>> {
    if app.machine_found_loading {
        return Some(Line::from(Span::styled(
            format!(" {} loading the inventory…", spinner(app)),
            theme::dim(),
        )));
    }
    app.machine_found_errors
        .first()
        .map(|e| Line::from(Span::styled(format!(" ⚠ {e}"), theme::fg(ERR))))
}
