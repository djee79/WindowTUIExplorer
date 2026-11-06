//! The bulk rename panel (docs/screens/12-bulk-rename.txt): the two fields, the option
//! toggles, the live Original │ Renamed │ Status table, the counts, and the token help.

use super::columns::{gutter_cell, gutter_width};
use super::theme::{self, ACCENT, BORDER, DIM, ERR, FG, NET, OK, PANEL_BG, SEL_BG, WARN};
use super::{truncate, width};
use crate::app::{App, RenameField};
use crate::rename::plan::Status;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

/// Lines under the table: separator, counts, blank, two notes, blank, two token lines.
const FOOTER: usize = 8;

pub fn draw(f: &mut Frame, body: Rect, app: &mut App) {
    let (number, relative) = (app.number, app.relative_number);
    let presets: Vec<(String, String)> = app
        .rename_presets
        .iter()
        .map(|p| (p.name.clone(), format!("{}  →  {}", p.search, p.replace)))
        .collect();
    let Some(p) = app.rename_panel.as_mut() else {
        return;
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER))
        .style(Style::new().bg(PANEL_BG).fg(FG))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(BORDER)),
            Span::styled("Bulk rename", theme::bold(ACCENT)),
            Span::styled(
                format!("  {} items in {} ", p.sources.len(), p.dir),
                theme::dim(),
            ),
        ]));
    let inner = block.inner(body);
    f.render_widget(Clear, body);
    f.render_widget(block, body);
    let w = inner.width as usize;

    let field = |label: &str, text: String, active: bool, hint: &str| {
        let mut spans = vec![Span::styled(
            format!("  {label:<14}"),
            if active {
                theme::bold(ACCENT)
            } else {
                theme::dim()
            },
        )];
        spans.push(Span::styled(text.clone(), theme::fg(FG)));
        if active {
            spans.push(Span::styled("▏", theme::fg(ACCENT)));
        }
        let pad = w.saturating_sub(16 + width(&text) + 1 + width(hint) + 2);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(hint.to_string(), theme::dim()));
        Line::from(spans)
    };
    let typing = !p.normal;
    let mut lines = vec![
        Line::default(),
        field(
            "Search for",
            p.search.text(),
            typing && p.field == RenameField::Search,
            "Tab switches field · ^p/^n history",
        ),
        Line::default(),
        field(
            "Replace with",
            p.replace.text(),
            typing && p.field == RenameField::Replace,
            "${ and $ tokens below",
        ),
        Line::default(),
    ];
    // The toggles, as in PowerRename, with their keys.
    let o = &p.options;
    let flag = |on: bool| if on { "[x]" } else { "[ ]" };
    let opts1 = format!(
        "  r regex {}    b boost {}    a match all {}    c case {}    n apply to {}    f files {}    d folders {}    s subfolders {}",
        flag(o.regex),
        flag(o.boost),
        flag(o.match_all),
        flag(o.case),
        o.apply_to.word(),
        flag(o.files),
        flag(o.folders),
        flag(o.subfolders)
    );
    let opts2 = format!(
        "  t format {}    e enumerate {}    x random {}    m dates from {}    o only changed {}",
        o.format.word(),
        flag(o.enumerate),
        flag(o.random),
        o.dates.word(),
        flag(o.only_changed)
    );
    let opt_style = if p.normal {
        theme::fg(FG)
    } else {
        theme::dim()
    };
    lines.push(Line::from(Span::styled(truncate(&opts1, w), opt_style)));
    lines.push(Line::from(Span::styled(truncate(&opts2, w), opt_style)));
    match &p.naming {
        Some(ed) => lines.push(Line::from(vec![
            Span::styled("  Save preset as  ", theme::bold(ACCENT)),
            Span::styled(ed.text(), theme::fg(FG)),
            Span::styled("▏", theme::fg(ACCENT)),
            Span::styled("   Enter saves (same name replaces) · Esc", theme::dim()),
        ])),
        None => lines.push(Line::default()),
    }
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(w.saturating_sub(4))),
        theme::fg(BORDER),
    )));

    // The table.
    let shown = p.shown();
    let gw = gutter_width(shown.len(), number, relative);
    let col = (w.saturating_sub(gw + 6)) / 3;
    lines.push(Line::from(Span::styled(
        format!(
            "  {}{:<col$}  {:<col$}  Status",
            " ".repeat(if gw > 0 { gw + 1 } else { 0 }),
            "Original",
            "Renamed"
        ),
        theme::dim(),
    )));
    let rows_h = (inner.height as usize).saturating_sub(lines.len() + FOOTER);
    if p.cursor < p.offset {
        p.offset = p.cursor;
    } else if rows_h > 0 && p.cursor >= p.offset + rows_h {
        p.offset = p.cursor + 1 - rows_h;
    }
    match &p.plan {
        Err(e) => lines.push(Line::from(Span::styled(format!("  {e}"), theme::fg(ERR)))),
        Ok(plan) => {
            for (n, &ri) in shown.iter().enumerate().skip(p.offset).take(rows_h) {
                let Some(row) = plan.rows.get(ri) else {
                    continue;
                };
                let Some(src) = p.sources.get(row.source) else {
                    continue;
                };
                let (status, style) = match &row.status {
                    Status::Rename => ("ok".to_string(), theme::fg(OK)),
                    Status::Warn(w) => (format!("⚠ {w}"), theme::fg(WARN)),
                    Status::Unchanged => ("·  unchanged".to_string(), theme::fg(DIM)),
                    Status::Excluded => ("–  excluded".to_string(), theme::fg(DIM)),
                    Status::Blocked(w) => (format!("✕ {w}"), theme::fg(ERR)),
                };
                let renamed_style = match row.status {
                    Status::Rename | Status::Warn(_) => theme::fg(NET),
                    Status::Blocked(_) => theme::fg(ERR),
                    _ => theme::fg(DIM),
                };
                let sel = p.normal && n == p.cursor;
                let mut spans = vec![Span::raw("  ")];
                if gw > 0 {
                    spans.push(gutter_cell(n, p.cursor, gw, number, relative));
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(
                    format!("{:<col$}  ", truncate(&src.name, col)),
                    if src.excluded {
                        theme::fg(DIM)
                    } else {
                        theme::fg(FG)
                    },
                ));
                spans.push(Span::styled(
                    format!("{:<col$}  ", truncate(&row.new_name, col)),
                    renamed_style,
                ));
                spans.push(Span::styled(truncate(&status, col), style));
                let mut line = Line::from(spans);
                if sel {
                    line = line.style(Style::new().bg(SEL_BG));
                }
                lines.push(line);
            }
        }
    }
    let footer_at = (inner.height as usize).saturating_sub(FOOTER);
    while lines.len() < footer_at {
        lines.push(Line::default());
    }
    lines.truncate(footer_at);
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(w.saturating_sub(4))),
        theme::fg(BORDER),
    )));
    let counts = match &p.plan {
        Ok(plan) => {
            let mut s = format!(
                "  {} will be renamed  ·  {} unchanged",
                plan.renames, plan.unchanged
            );
            if plan.excluded > 0 {
                s.push_str(&format!("  ·  {} excluded", plan.excluded));
            }
            if plan.blocked > 0 {
                s.push_str(&format!(
                    "  ·  {} blocked: fix it or exclude it (Space) before Enter can apply",
                    plan.blocked
                ));
            }
            s
        }
        Err(_) => "  fix the search to see the preview".to_string(),
    };
    let mut counts = counts;
    if p.walking {
        counts.push_str("  ·  walking the subfolders…");
    }
    if p.meta_reading {
        counts.push_str("  ·  reading photo / PDF info…");
    }
    let blocked = p.plan.as_ref().is_ok_and(|pl| pl.blocked > 0);
    lines.push(Line::from(Span::styled(
        counts,
        if blocked {
            theme::fg(ERR)
        } else {
            theme::fg(FG)
        },
    )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Two-phase apply handles swaps and case-only renames · u undoes it afterwards · ^s save preset · ^o presets",
        theme::dim(),
    )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Tokens: ${padding=4;start=10;increment=2}  ${rstringalnum=6}  ${ruuidv4}  $YYYY $MM $DD $hh $mm  $CAMERA_MODEL",
        theme::dim(),
    )));
    lines.push(Line::from(Span::styled(
        "  sentier extras:  ${parent}  ${name}  ${ext}  ${n;reset=folder}   (e enumerate, x random)",
        theme::dim(),
    )));
    f.render_widget(Paragraph::new(lines), inner);

    // `^o`: the preset list over the table.
    if let Some(l) = &p.presets {
        let h = (presets.len() as u16 + 2)
            .min(inner.height.saturating_sub(4))
            .max(3);
        let pw = (inner.width.saturating_sub(8)).min(90);
        let area = Rect {
            x: inner.x + (inner.width.saturating_sub(pw)) / 2,
            y: inner.y + (inner.height.saturating_sub(h)) / 2,
            width: pw,
            height: h,
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::fg(ACCENT))
            .style(Style::new().bg(PANEL_BG).fg(FG))
            .title(Span::styled(" Rename presets ", theme::bold(ACCENT)))
            .title_bottom(Span::styled(
                if l.delete_armed {
                    " x again deletes it · any other key keeps it "
                } else {
                    " j/k · Enter load · x delete · Esc "
                },
                if l.delete_armed {
                    theme::fg(ERR)
                } else {
                    theme::dim()
                },
            ));
        let pin = block.inner(area);
        let rows = pin.height as usize;
        let first = l.cursor.saturating_sub(rows.saturating_sub(1));
        let nw = presets
            .iter()
            .map(|(n, _)| width(n))
            .max()
            .unwrap_or(0)
            .min(30);
        let items: Vec<Line> = presets
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, (name, what))| {
                let line = Line::from(vec![
                    Span::styled(format!(" {:<nw$}  ", truncate(name, nw)), theme::fg(FG)),
                    Span::styled(
                        truncate(what, (pin.width as usize).saturating_sub(nw + 4)),
                        theme::dim(),
                    ),
                ]);
                if i == l.cursor {
                    line.style(Style::new().bg(SEL_BG))
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(items), pin);
    }
}
