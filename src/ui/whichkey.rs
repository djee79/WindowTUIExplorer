//! The which-key popup (docs/screens/06-which-key.txt): after a prefix key waits past
//! the keymap timeout, list every continuation at the bottom of the body.

use super::theme::{self, ACCENT, BORDER, PANEL_BG};
use super::{truncate, width};
use crate::app::App;
use crate::keymap::{Action, Continuation, Key, fmt_keys};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const KEY_W: usize = 8;
const COL_W: usize = 48;

/// After `'`: every pin with its key, so the popup doubles as a quick pin menu. After
/// `m`: the keys already taken (pressing one moves it here, like a vim mark).
fn char_rows(app: &App) -> Vec<Continuation> {
    let action = app.keymap.char_action(&app.keys_pending);
    // (key, name, what it opens) for pins and saved views.
    let mut keyed: Vec<(char, String, String)> =
        app.pins
            .pins
            .iter()
            .filter_map(|p| Some((p.key_char()?, p.name.clone(), p.detail())))
            .chain(app.pins.views.iter().filter_map(|v| {
                Some((v.key_char()?, v.name.clone(), format!("[view] {}", v.query)))
            }))
            .collect();
    keyed.sort_by_key(|(c, _, _)| *c);
    let row = |c: char, desc: String| Continuation {
        key: Key::plain(KeyCode::Char(c)),
        desc,
        group: false,
    };
    match action {
        Some(Action::JumpMark(_)) => keyed
            .into_iter()
            .map(|(c, name, what)| row(c, format!("{name}  {what}")))
            .collect(),
        Some(Action::SetMark(_)) => keyed
            .into_iter()
            .map(|(c, name, _)| row(c, format!("taken: {name}")))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    let mut rows = app.keymap.continuations(&app.keys_pending);
    rows.extend(char_rows(app));
    if rows.is_empty() {
        let hint = match app.keymap.char_action(&app.keys_pending) {
            Some(Action::JumpMark(_)) => "no pins with a key yet (m{key} in a folder)",
            _ => "any letter or digit",
        };
        rows.push(Continuation {
            key: crate::keymap::ANY_CHAR,
            desc: hint.to_string(),
            group: false,
        });
    }
    let inner_w = (body.width as usize).saturating_sub(2);
    let cols = (inner_w / COL_W).clamp(1, rows.len().max(1));
    let per_col = rows.len().div_ceil(cols).max(1);
    let h = (per_col as u16 + 2).min(body.height);
    let area = Rect {
        y: body.y + body.height - h,
        height: h,
        ..body
    };

    let lines: Vec<Line> = (0..per_col)
        .map(|r| {
            let mut spans = Vec::new();
            for c in 0..cols {
                let Some(row) = rows.get(c * per_col + r) else {
                    break;
                };
                let key = row.key.notation();
                spans.push(Span::styled(
                    format!("{}{key}", " ".repeat(KEY_W.saturating_sub(width(&key)))),
                    theme::bold(ACCENT),
                ));
                spans.push(Span::styled(" → ", theme::dim()));
                let desc = if row.group {
                    format!("+{}", row.desc)
                } else {
                    row.desc.clone()
                };
                let desc = truncate(&desc, COL_W - KEY_W - 4);
                let style = if row.group {
                    theme::fg(ACCENT)
                } else {
                    theme::fg(theme::FG)
                };
                let pad = (COL_W - KEY_W - 3).saturating_sub(width(&desc));
                spans.push(Span::styled(desc, style));
                spans.push(Span::raw(" ".repeat(pad)));
            }
            Line::from(spans)
        })
        .collect();

    let title = format!(
        "{}{}",
        app.count.map(|c| c.to_string()).unwrap_or_default(),
        fmt_keys(&app.keys_pending, app.keymap.leader)
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(BORDER))
        .style(Style::new().bg(PANEL_BG).fg(theme::FG))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(BORDER)),
            Span::styled(title, theme::bold(ACCENT)),
            Span::raw(" "),
        ]))
        .title(
            Line::from(Span::styled(" Esc cancel · Backspace up ", theme::dim())).right_aligned(),
        );
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
