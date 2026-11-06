//! The `O` menu (docs/screens/09-open-with.txt): the rule's openers, then Windows' Open
//! with… and Properties. Missing programs are greyed with "not found"; the opener used
//! last for this extension says `★ last`.

use super::theme::{self, ACCENT, BORDER, DIMMED, FG, PANEL_BG, SEL_BG, WARN};
use super::{truncate, width};
use crate::app::App;
use crate::open::Section;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    let Some(m) = app.open_menu.as_ref() else {
        return;
    };
    let shown = m.shown();
    let w = 68.min(body.width);
    let sections = 2;
    let h =
        (shown.len() as u16 + 4 + sections + u16::from(!m.filter.is_empty())).clamp(6, body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height.saturating_sub(h)) / 3,
        width: w,
        height: h,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER))
        .style(Style::new().bg(PANEL_BG).fg(FG))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(BORDER)),
            Span::styled("Open ", theme::dim()),
            Span::styled(m.title.clone(), theme::bold(ACCENT)),
            Span::styled(" with ", theme::dim()),
        ]))
        .title_bottom(Line::from(Span::styled(
            " 1-9 pick · j/k Enter · type to filter · Esc ",
            theme::dim(),
        )));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let iw = inner.width as usize;

    let mut lines = Vec::new();
    if !m.filter.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  filter: ", theme::dim()),
            Span::styled(m.filter.clone(), theme::bold(FG)),
        ]));
    }
    let mut section = None;
    for (n, &i) in shown.iter().enumerate() {
        let Some(item) = m.items.get(i) else { continue };
        if section != Some(item.section) {
            if section.is_some() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", "─".repeat(iw.saturating_sub(4))),
                    theme::fg(BORDER),
                )));
            }
            if item.section == Section::Configured {
                let from = m
                    .pattern
                    .as_deref()
                    .map_or_else(|| "configured".to_string(), |p| format!("configured ({p})"));
                lines.push(Line::from(Span::styled(format!("  {from}"), theme::dim())));
            }
            section = Some(item.section);
        }
        let sel = n == m.cursor;
        let num = if n < 9 && m.filter.is_empty() {
            format!("{}", n + 1)
        } else {
            " ".to_string()
        };
        let right = if item.last {
            "★ last".to_string()
        } else {
            item.hint.clone()
        };
        let label = truncate(&item.label, 24);
        let dw = iw.saturating_sub(4 + 3 + 24 + width(&right) + 3);
        let detail = truncate(&item.detail, dw);
        let pad = iw.saturating_sub(5 + 24 + width(&detail) + width(&right) + 2);
        let (main, dim) = if item.missing {
            (theme::fg(DIMMED), theme::fg(DIMMED))
        } else {
            (theme::fg(FG), theme::dim())
        };
        let mut row = Line::from(vec![
            Span::styled(if sel { " ▌" } else { "  " }, theme::bold(ACCENT)),
            Span::styled(format!("{num:>1}  "), theme::fg(ACCENT)),
            Span::styled(format!("{label:<24}"), main),
            Span::styled(detail, dim),
            Span::raw(" ".repeat(pad)),
            Span::styled(
                right,
                if item.last {
                    theme::fg(WARN)
                } else {
                    theme::dim()
                },
            ),
        ]);
        if sel {
            row = row.style(Style::new().bg(SEL_BG));
        }
        lines.push(row);
    }
    if shown.is_empty() {
        lines.push(Line::from(Span::styled("  no match", theme::dim())));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
