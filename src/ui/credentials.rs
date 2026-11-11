//! The credential prompt (docs/screens/11-credentials.txt): a box over the body with the
//! target, Windows' reason, User, a masked Password and the Remember checkbox.

use super::theme;
use super::{spinner, truncate, width};
use crate::app::App;
use crate::app::auth::Field;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const W: u16 = 84;
const H: u16 = 19;

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    let Some(p) = app.cred.as_ref() else { return };
    let w = W.min(body.width);
    let h = H.min(body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height - h) / 3,
        width: w,
        height: h,
    };
    let title_right = p
        .profile
        .as_ref()
        .map(|name| format!(" profile: {name} "))
        .unwrap_or_default();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(theme::pal().border))
        .style(Style::new().bg(theme::pal().panel_bg).fg(theme::pal().fg))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(theme::pal().border)),
            Span::styled("Credentials needed", theme::bold(theme::pal().accent)),
            Span::raw(" "),
        ]))
        .title(Line::from(Span::styled(title_right, theme::dim())).right_aligned());
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let iw = inner.width as usize;

    let label = |text: &str, focused: bool| {
        let style = if focused {
            theme::bold(theme::pal().accent)
        } else {
            theme::dim()
        };
        Span::styled(format!("  {text:<11}"), style)
    };
    let caret = Span::styled("▏", theme::fg(theme::pal().accent));

    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!("  {}", truncate(&p.target, iw.saturating_sub(4))),
            theme::bold(theme::pal().fg),
        )),
        Line::from(Span::styled(
            format!("  {}", truncate(&p.reason, iw.saturating_sub(4))),
            theme::fg(if p.tries > 0 {
                theme::pal().err
            } else {
                theme::pal().warn
            }),
        )),
        Line::default(),
    ];
    // User
    let user = p.user.text();
    let mut row = vec![label("User", p.focus == Field::User), Span::raw(user)];
    if p.focus == Field::User && !p.busy {
        row.push(caret.clone());
    }
    lines.push(Line::from(row));
    lines.push(Line::default());
    // Password: one ● per character, never the characters.
    let dots = "●".repeat(p.password_len().min(iw.saturating_sub(16)));
    let mut row = vec![
        label("Password", p.focus == Field::Password),
        Span::raw(dots),
    ];
    if p.focus == Field::Password && !p.busy {
        row.push(caret);
    }
    lines.push(Line::from(row));
    lines.push(Line::default());
    // Remember
    let check = if p.remember { "[x]" } else { "[ ]" };
    let style = if p.focus == Field::Remember {
        theme::bold(theme::pal().accent)
    } else {
        theme::fg(theme::pal().fg)
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{check} Remember in Windows Credential Manager"),
            style,
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "      off by default · Windows would then connect without asking",
        theme::dim(),
    )));
    lines.push(Line::default());
    if p.busy {
        lines.push(Line::from(Span::styled(
            format!("  {} connecting…   Esc abandon", spinner(app)),
            theme::fg(theme::pal().accent),
        )));
    } else if p.conflict {
        // Error 1219: Windows allows one account per server. The two real fixes.
        let server = p.server();
        for (key, text) in [
            (
                "d",
                format!("disconnect the existing connection(s) to {server}, then connect"),
            ),
            (
                "a",
                format!("connect through {server}'s IP address instead"),
            ),
        ] {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key} "), theme::bold(theme::pal().accent)),
                Span::styled(
                    truncate(&text, iw.saturating_sub(6)),
                    theme::fg(theme::pal().fg),
                ),
            ]));
        }
    } else {
        let keys = "Tab next field · Space toggle";
        let buttons = "  [ Enter Connect ]   [ Esc Cancel ]";
        let pad = iw.saturating_sub(width(buttons) + width(keys) + 2);
        lines.push(Line::from(vec![
            Span::styled(
                buttons,
                theme::fg(theme::pal().fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad)),
            Span::styled(keys, theme::dim()),
        ]));
    }
    lines.push(Line::default());
    for text in [
        "Connection lasts until you log off; robocopy jobs and Explorer reuse it.",
        "The password is never stored, logged or put on a command line by sentier.",
    ] {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(text, iw.saturating_sub(4))),
            theme::dim(),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
