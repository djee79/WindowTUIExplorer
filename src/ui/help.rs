//! `g?` overlay, built from the live keymap (defaults + keymap.toml), so it can't drift
//! from what the keys actually do.

use super::theme::{self, ACCENT, BORDER, PANEL_BG};
use super::{truncate, width};
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const KEY_W: usize = 14;
const DESC_W: usize = 34;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let mut rows = app.keymap.help_rows();
    rows.push(("Ctrl-c".to_string(), "quit at once".to_string()));
    rows.push((
        "prompts".to_string(),
        "Esc normal · Esc Esc cancel".to_string(),
    ));

    let col_w = KEY_W + DESC_W + 2;
    let avail_h = area.height.saturating_sub(2).max(1) as usize;
    let max_cols = ((area.width as usize).saturating_sub(2) / col_w).max(1);
    let cols = rows.len().div_ceil(avail_h).clamp(1, max_cols);
    let per_col = rows.len().div_ceil(cols);

    let lines: Vec<Line> = (0..per_col.min(avail_h))
        .map(|r| {
            let mut spans = Vec::new();
            for c in 0..cols {
                let Some((k, d)) = rows.get(c * per_col + r) else {
                    break;
                };
                let k = truncate(k, KEY_W - 1);
                spans.push(Span::styled(
                    format!(" {k}{}", " ".repeat(KEY_W - 1 - width(&k))),
                    theme::bold(ACCENT),
                ));
                let d = truncate(d, DESC_W);
                let pad = DESC_W - width(&d) + 1;
                spans.push(Span::raw(format!("{d}{}", " ".repeat(pad))));
            }
            Line::from(spans)
        })
        .collect();

    let h = lines.len() as u16 + 2;
    let w = (cols * col_w) as u16 + 2;
    let [popup] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER))
        .style(Style::new().bg(PANEL_BG).fg(theme::FG))
        .title(Line::from(Span::styled(" keys ", theme::bold(ACCENT))))
        .title_bottom(
            Line::from(Span::styled(
                " any key closes · remap in %APPDATA%\\sentier\\keymap.toml ",
                theme::dim(),
            ))
            .right_aligned(),
        );
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}
