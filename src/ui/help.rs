//! `?` key overlay.

use super::theme::{self, ACCENT, BORDER, PANEL_BG};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const KEYS: &[(&str, &str)] = &[
    ("j k  ↓ ↑", "move"),
    ("h  ←  ⌫", "parent"),
    ("l  →  ⏎", "enter dir / open file"),
    ("gg  G", "top / bottom"),
    ("^d  ^u", "half page down / up"),
    ("gl", "Locations root"),
    ("~", "home"),
    ("/", "filter this directory"),
    (".", "show hidden/system files"),
    ("R", "retry / refresh"),
    ("q", "quit"),
    ("Q", "quit and cd here (shell wrapper)"),
    ("?", "this help"),
];

pub fn draw(f: &mut Frame, area: Rect) {
    let h = KEYS.len() as u16 + 2;
    let [popup] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(46)])
        .flex(Flex::Center)
        .areas(popup);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!(" {k:<10}"), theme::bold(ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER))
        .style(Style::new().bg(PANEL_BG).fg(theme::FG))
        .title(Line::from(Span::styled(" keys ", theme::bold(ACCENT))))
        .title_bottom(Line::from(Span::styled(" any key closes ", theme::dim())).right_aligned());
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}
