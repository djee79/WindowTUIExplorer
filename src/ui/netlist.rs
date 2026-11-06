//! `:net list`: the live SMB connections of this logon session, the account each uses,
//! and which ones sentier made.

use super::theme::{self, ACCENT, BORDER, FG, PANEL_BG};
use super::truncate_left;
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    let Some(list) = app.net_listing.as_ref() else {
        return;
    };
    let w = 100.min(body.width);
    let h = (list.rows.len() as u16 + 7).clamp(8, body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height - h) / 3,
        width: w,
        height: h,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER))
        .style(Style::new().bg(PANEL_BG).fg(FG))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(BORDER)),
            Span::styled("Network connections", theme::bold(ACCENT)),
            Span::raw(" "),
        ]))
        .title(Line::from(Span::styled(" Esc close ", theme::dim())).right_aligned());
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let iw = inner.width as usize;
    let remote_w = iw.saturating_sub(4 + 4 + 30 + 10).max(20);

    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!("  {:<4}{:<remote_w$}  {:<30}", "", "Remote", "User"),
            theme::dim(),
        )),
    ];
    if list.rows.is_empty() {
        lines.push(Line::from(Span::styled("  no connections", theme::dim())));
    }
    for (c, made) in &list.rows {
        let local = c.local.clone().unwrap_or_default();
        let user = c
            .user
            .clone()
            .unwrap_or_else(|| "(current login)".to_string());
        lines.push(Line::from(vec![
            Span::styled(format!("  {local:<4}"), theme::fg(ACCENT)),
            Span::styled(
                format!("{:<remote_w$}", truncate_left(&c.remote, remote_w)),
                theme::fg(theme::NET),
            ),
            Span::styled(format!("  {user:<30}"), theme::fg(FG)),
            Span::styled(if *made { "  sentier" } else { "" }, theme::dim()),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  :net drop <server> disconnects (not drive letters) · :net drop-all the sentier ones",
        theme::dim(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}
