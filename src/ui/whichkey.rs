//! The which-key popup (docs/screens/06-which-key.txt): after a prefix key waits past
//! the keymap timeout, list every continuation at the bottom of the body.

use super::theme;
use super::{truncate, width};
use crate::app::App;
use crate::fs::EntryKind;
use crate::keymap::{Action, Continuation, Key, Lookup, fmt_keys};
use crate::pins::KeyOwner;
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const KEY_W: usize = 8;
const COL_W: usize = 48;

/// After `'`: every pin with its key, so the popup doubles as a quick pin menu; keys
/// sharing a first letter (`'dd`, `'dl`) show as one `+d` group. After `'d`: that
/// group's second letters. After `m`: the keys already taken (pressing one moves it
/// here, like a vim mark).
fn char_rows(app: &App) -> Vec<Continuation> {
    // (key, name, what it opens) for pins and saved views.
    let mut keyed: Vec<(String, String, String)> = app
        .pins
        .keyed()
        .map(|(o, k)| {
            let (name, what) = match o {
                KeyOwner::Pin(i) => app.pins.pins.get(i).map(|p| (p.name.clone(), p.detail())),
                KeyOwner::View(i) => app
                    .pins
                    .views
                    .get(i)
                    .map(|v| (v.name.clone(), format!("[view] {}", v.query))),
            }
            .unwrap_or_default();
            (k.to_string(), name, what)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let row = |c: char, desc: String, group: bool| Continuation {
        key: Key::plain(KeyCode::Char(c)),
        desc,
        group,
    };
    if let Some(first) = app.mark_prefix {
        return keyed
            .into_iter()
            .filter_map(|(k, name, what)| {
                let second = k.strip_prefix(first)?.chars().next()?;
                Some(row(second, format!("{name}  {what}"), false))
            })
            .collect();
    }
    let jump = match app.keymap.char_action(&app.keys_pending) {
        Some(Action::JumpMark(_)) => true,
        Some(Action::SetMark(_)) => false,
        _ => return Vec::new(),
    };
    let mut rows: Vec<Continuation> = Vec::new();
    for (k, name, what) in keyed {
        let mut chars = k.chars();
        let (Some(first), second) = (chars.next(), chars.next()) else {
            continue;
        };
        if second.is_none() {
            let desc = if jump {
                format!("{name}  {what}")
            } else {
                format!("taken: {name}")
            };
            rows.push(row(first, desc, false));
            continue;
        }
        // A two-letter key: one group row per first letter, naming its pins.
        let group = Key::plain(KeyCode::Char(first));
        match rows.iter_mut().find(|r| r.key == group) {
            Some(r) => r.desc = format!("{} · {name}", r.desc),
            None => rows.push(row(first, name, true)),
        }
    }
    rows
}

/// What `dd` does on the Locations entry under the cursor: never anything to files.
fn locations_delete_desc(app: &App) -> &'static str {
    match app.selected().map(|e| e.kind) {
        Some(EntryKind::Pin) => "unpin (asks; the folder itself stays)",
        Some(EntryKind::Share) => "forget this share (files untouched)",
        Some(EntryKind::Drive(_)) => "nothing: drives can't be removed",
        _ => "remove from this list (files untouched)",
    }
}

/// The popup's rows: the continuations of the pending keys, then the pin keys.
fn rows(app: &App) -> Vec<Continuation> {
    let mut rows = if app.mark_prefix.is_some() {
        Vec::new()
    } else {
        app.keymap.continuations(&app.keys_pending)
    };
    // In Locations `dd` only edits the list: say so rather than "Recycle Bin".
    if app.at_root() {
        for row in &mut rows {
            let mut seq = app.keys_pending.clone();
            seq.push(row.key);
            if let Lookup::Exact {
                action: Action::Delete,
                ..
            } = app.keymap.lookup(&seq)
            {
                row.desc = locations_delete_desc(app).to_string();
            }
        }
    }
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
    rows
}

/// `n` rows in a popup `width` wide at the bottom of `body`: its box, columns, and rows
/// per column.
fn layout(body: Rect, width: u16, n: usize) -> (Rect, usize, usize) {
    let inner_w = (width as usize).saturating_sub(2);
    let cols = (inner_w / COL_W).clamp(1, n.max(1));
    let per_col = n.div_ceil(cols).max(1);
    let h = (per_col as u16 + 2).min(body.height);
    let area = Rect {
        y: body.y + body.height - h,
        height: h,
        width,
        ..body
    };
    (area, cols, per_col)
}

/// The popup's box, columns and rows per column. It spans the body, unless that would
/// cover the previewed `picture`: then it keeps to the left of the preview column when
/// there is room for a column of keys. Covering a picture means taking it off the
/// screen and sending it again afterwards, which is what makes a prefix key feel slow.
fn placed(body: Rect, n: usize, picture: Option<Rect>) -> (Rect, usize, usize) {
    let full = layout(body, body.width, n);
    let Some(picture) = picture.filter(|p| p.intersects(full.0)) else {
        return full;
    };
    // Up to the preview panel's border (the picture sits a margin inside it).
    let left = picture.x.saturating_sub(body.x + 2);
    if (left as usize) < COL_W + 2 {
        return full;
    }
    layout(body, left, n)
}

/// Where the popup goes in `body` when `picture` is on screen (see [`placed`]).
pub fn area(body: Rect, app: &App, picture: Option<Rect>) -> Rect {
    placed(body, rows(app).len(), picture).0
}

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    let rows = rows(app);
    let (area, cols, per_col) = placed(body, rows.len(), app.image_drawn);

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
                    theme::bold(theme::pal().accent),
                ));
                spans.push(Span::styled(" → ", theme::dim()));
                let desc = if row.group {
                    format!("+{}", row.desc)
                } else {
                    row.desc.clone()
                };
                let desc = truncate(&desc, COL_W - KEY_W - 4);
                let style = if row.group {
                    theme::fg(theme::pal().accent)
                } else {
                    theme::fg(theme::pal().fg)
                };
                let pad = (COL_W - KEY_W - 3).saturating_sub(width(&desc));
                spans.push(Span::styled(desc, style));
                spans.push(Span::raw(" ".repeat(pad)));
            }
            Line::from(spans)
        })
        .collect();

    let title = match app.mark_prefix {
        Some(c) => format!("'{c}"),
        None => format!(
            "{}{}",
            app.count.map(|c| c.to_string()).unwrap_or_default(),
            fmt_keys(&app.keys_pending, app.keymap.leader)
        ),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(theme::pal().border))
        .style(Style::new().bg(theme::pal().panel_bg).fg(theme::pal().fg))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(theme::pal().border)),
            Span::styled(title, theme::bold(theme::pal().accent)),
            Span::raw(" "),
        ]))
        .title(
            Line::from(Span::styled(" Esc cancel · Backspace up ", theme::dim())).right_aligned(),
        );
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_popup_keeps_clear_of_a_picture() {
        let body = Rect::new(0, 1, 200, 50);
        // Nothing previewed: the whole width, at the bottom.
        let (full, cols, per_col) = placed(body, 4, None);
        assert_eq!((full.x, full.width, full.bottom()), (0, 200, 51));
        assert_eq!((cols, per_col, full.height), (4, 1, 3));
        // A picture that ends above the popup is no reason to move.
        let short = Rect::new(118, 2, 80, 20);
        assert_eq!(placed(body, 4, Some(short)).0, full);
        // A tall one: the popup stays left of the preview column, with more rows.
        let tall = Rect::new(118, 2, 60, 48);
        let (narrow, cols, per_col) = placed(body, 4, Some(tall));
        assert!(!narrow.intersects(tall));
        assert_eq!((narrow.x, narrow.width, narrow.bottom()), (0, 116, 51));
        assert_eq!((cols, per_col), (2, 2));
        // No room beside it (the full-screen preview): over the picture, as before.
        let wide = Rect::new(2, 2, 190, 48);
        assert_eq!(placed(body, 4, Some(wide)).0, full);
    }
}
