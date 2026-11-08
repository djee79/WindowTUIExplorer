//! A yes/no question as a popup (Yazi-style): a title, the question, what it touches
//! (the first few, "… and N more"), and [ Yes ] [ No ]. Destructive ones are red and
//! start on No.

use super::theme::{self, ACCENT, BORDER, ERR, FG, ON_ACCENT, PANEL_BG};
use super::{truncate, width};
use crate::app::DialogView;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

/// Items listed before "… and N more".
const MAX_ITEMS: usize = 8;

/// Splits `s` into lines of at most `w` cells, at spaces where it can.
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        if !line.is_empty() && width(&line) + 1 + width(word) > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

pub fn draw(f: &mut Frame, body: Rect, d: &DialogView) {
    let w = 64.min(body.width.saturating_sub(4)).max(30);
    let iw = (w as usize).saturating_sub(4);
    let color = if d.danger { ERR } else { ACCENT };

    let mut lines: Vec<Line> = vec![Line::default()];
    for l in wrap(&d.message, iw) {
        lines.push(Line::from(Span::styled(format!(" {l}"), theme::fg(FG))));
    }
    if !d.items.is_empty() {
        lines.push(Line::default());
        for it in d.items.iter().take(MAX_ITEMS) {
            lines.push(Line::from(vec![
                Span::styled("  • ", theme::fg(color)),
                Span::styled(truncate(it, iw.saturating_sub(3)), theme::fg(FG)),
            ]));
        }
        if d.items.len() > MAX_ITEMS {
            lines.push(Line::from(Span::styled(
                format!("    … and {} more", d.items.len() - MAX_ITEMS),
                theme::dim(),
            )));
        }
    }
    lines.push(Line::default());
    // [ Yes ]   [ No ], the one Enter presses highlighted.
    let button = |label: &str, on: bool, danger: bool| {
        let style = if on {
            Style::new()
                .bg(if danger { ERR } else { ACCENT })
                .fg(ON_ACCENT)
                .add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            theme::fg(FG)
        };
        Span::styled(format!("  {label}  "), style)
    };
    let [yes, no] = d.buttons;
    let buttons_w = width(yes) + 4 + 3 + width(no) + 4;
    let pad = (iw.saturating_sub(buttons_w)) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad + 1)),
        button(yes, d.yes, d.danger),
        Span::raw("   "),
        button(no, !d.yes, false),
    ]));
    lines.push(Line::default());

    let h = (lines.len() as u16 + 2).min(body.height);
    let area = Rect {
        x: body.x + (body.width.saturating_sub(w)) / 2,
        y: body.y + (body.height.saturating_sub(h)) / 3,
        width: w,
        height: h,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(if d.danger { ERR } else { BORDER }))
        .style(Style::new().bg(PANEL_BG).fg(FG))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(BORDER)),
            Span::styled(d.title.clone(), theme::bold(color)),
            Span::raw(" "),
        ]))
        .title_bottom(
            Line::from(Span::styled(format!(" {} ", d.keys), theme::dim())).right_aligned(),
        );
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
