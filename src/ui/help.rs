//! `g?`: the key help (docs/HELP_OVERLAY_SPEC.md). Content and layout come from
//! `crate::help` (built from the live keymap); this file only draws it: key chips,
//! sections in balanced columns, the `:` commands beside them, the filter box, the legend.

use super::theme;
use super::{truncate, width};
use crate::app::App;
use crate::help::{self, Chip, CmdRow, Column, HelpData, KeyRow, Rows};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

/// Inner horizontal padding of the overlay.
const PAD: u16 = 3;
/// Width of the filter box.
const FILTER_W: u16 = 60;

/// Colours the user may set (`[theme]` in config.toml).
struct Colors {
    key: Color,
    header: Color,
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let Some(h) = app.help.as_ref() else {
        return;
    };
    let colors = Colors {
        key: app
            .config
            .theme
            .key
            .as_deref()
            .and_then(theme::parse_hex)
            .unwrap_or(theme::pal().accent),
        header: app
            .config
            .theme
            .header
            .as_deref()
            .and_then(theme::parse_hex)
            .unwrap_or(theme::pal().dir),
    };
    let query = h.filter.text();
    let data = h.shown();

    // The overlay: a 1-cell margin, rounded border, titles on both edges.
    let outer = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if outer.width < 30 || outer.height < 8 {
        return;
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(theme::pal().border))
        .style(Style::new().bg(theme::pal().panel_bg).fg(theme::pal().fg))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(theme::pal().border)),
            Span::styled("help", theme::bold(colors.header)),
            Span::styled(
                format!(" · {} keys, {} commands ", h.data.keys, h.data.commands),
                theme::dim(),
            ),
        ]))
        .title(
            Line::from(Span::styled(" sentier ─", theme::bold(theme::pal().accent)))
                .right_aligned(),
        )
        .title_bottom(Line::from(Span::styled(
            match (h.typing, outer.width >= 130) {
                (true, true) => " Esc / ↵ leave the filter · C-n C-p scroll ",
                (true, false) => " Esc leaves the filter ",
                (false, true) => " Esc or any key closes · / filter · j k C-n C-p C-d C-u scroll ",
                (false, false) => " Esc closes · / filter · j k scroll ",
            },
            theme::dim(),
        )))
        .title_bottom(
            Line::from(vec![
                Span::styled(" remap in ", theme::dim()),
                Span::styled(
                    "%APPDATA%\\sentier\\keymap.toml",
                    theme::fg(theme::pal().ok),
                ),
                Span::raw(" "),
            ])
            .right_aligned(),
        );
    let inner = block.inner(outer);
    f.render_widget(Clear, outer);
    f.render_widget(block, outer);
    let content = Rect {
        x: inner.x + PAD,
        y: inner.y,
        width: inner.width.saturating_sub(2 * PAD),
        height: inner.height,
    };

    // Row 1–3: the filter box, and the legend to its right.
    let fw = FILTER_W.min(content.width);
    let filter_area = Rect {
        x: content.x,
        y: content.y,
        width: fw,
        height: 3,
    };
    let fblock = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(theme::fg(if h.typing {
            theme::pal().accent
        } else {
            theme::pal().border
        }))
        .style(Style::new().bg(theme::pal().bar_bg));
    let typed = if query.is_empty() && !h.typing {
        Line::from(vec![
            Span::styled("/ ", theme::bold(colors.key)),
            Span::styled("type to filter keys and commands", theme::dim()),
        ])
    } else {
        Line::from(vec![
            Span::styled("/ ", theme::bold(colors.key)),
            Span::styled(query.clone(), theme::fg(theme::pal().fg)),
            Span::styled(
                if h.typing { "▏" } else { "" },
                theme::fg(theme::pal().accent),
            ),
        ])
    };
    f.render_widget(Paragraph::new(typed).block(fblock), filter_area);
    let legend = legend_line(&colors);
    let lw = legend.width() as u16;
    if content.width > fw + lw + 2 {
        f.render_widget(
            Paragraph::new(legend).right_aligned(),
            Rect {
                x: content.x + fw,
                y: content.y + 1,
                width: content.width - fw,
                height: 1,
            },
        );
    }

    // The body: columns side by side, scrolled together.
    let body = Rect {
        x: content.x,
        y: content.y + 4,
        width: content.width,
        height: content.height.saturating_sub(4),
    };
    if data.sections.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("nothing matches \"{query}\""),
                theme::dim(),
            )),
            body,
        );
        return;
    }
    let cols = help::layout(&data, body.width as usize);
    let q = query.trim().to_lowercase();
    let col_lines: Vec<Vec<Line>> = cols
        .iter()
        .map(|c| column_lines(&data, c, &colors, &q))
        .collect();
    let total = col_lines.iter().map(Vec::len).max().unwrap_or(0);
    let rows = body.height as usize;
    h.max_scroll.set(total.saturating_sub(rows));
    h.page.set(rows);
    let scroll = h.scroll.min(total.saturating_sub(rows));
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for y in scroll..(scroll + rows).min(total) {
        let mut spans: Vec<Span> = Vec::new();
        for (ci, c) in cols.iter().enumerate() {
            if ci > 0 {
                if c.commands {
                    spans.push(Span::styled("   │   ", theme::fg(theme::pal().border)));
                } else {
                    spans.push(Span::raw(" ".repeat(help::GAP)));
                }
            }
            match col_lines[ci].get(y) {
                Some(l) => {
                    let w = l.width();
                    spans.extend(l.spans.iter().cloned());
                    spans.push(Span::raw(" ".repeat(c.width.saturating_sub(w))));
                }
                None => spans.push(Span::raw(" ".repeat(c.width))),
            }
        }
        lines.push(Line::from(spans));
    }
    if total > rows {
        let more = format!(" {}/{} ", (scroll + rows).min(total), total);
        f.render_widget(
            Paragraph::new(Span::styled(more, theme::dim())).right_aligned(),
            Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(1),
                width: inner.width,
                height: 1,
            },
        );
        lines.truncate(rows.saturating_sub(1).max(1));
    }
    f.render_widget(Paragraph::new(lines), body);
}

/// `␣ leader (Space)   C- Ctrl   A- Alt   ↵ Enter`.
fn legend_line(c: &Colors) -> Line<'static> {
    let k = |s: &'static str| Span::styled(s, theme::bold(c.key));
    let d = |s: &'static str| Span::styled(s, theme::dim());
    Line::from(vec![
        Span::styled("␣", theme::bold(theme::pal().net)),
        d(" leader (Space)   "),
        k("C-"),
        d(" Ctrl   "),
        k("A-"),
        d(" Alt   "),
        k("↵"),
        d(" Enter"),
    ])
}

/// Every line of one column: section headers, rows, a blank between sections.
fn column_lines(data: &HelpData, col: &Column, c: &Colors, q: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (n, &si) in col.sections.iter().enumerate() {
        let Some(s) = data.sections.get(si) else {
            continue;
        };
        if n > 0 {
            out.push(Line::default());
        }
        let color = if s.is_commands() {
            theme::pal().cmd
        } else {
            c.header
        };
        let tw = width(s.title);
        out.push(Line::from(vec![
            Span::styled(s.title.to_string(), theme::bold(color)),
            Span::raw(" "),
            Span::styled(
                "─".repeat(col.width.saturating_sub(tw + 1)),
                theme::fg(theme::pal().border),
            ),
        ]));
        match &s.rows {
            Rows::Keys(rows) => {
                for r in rows {
                    out.extend(key_row(r, col.width, c, q));
                }
            }
            Rows::Commands(rows) => {
                for r in rows {
                    out.extend(cmd_row(r, col.width, q));
                }
            }
        }
    }
    out
}

fn chip_spans(ch: &Chip, c: &Colors) -> Vec<Span<'static>> {
    let bg = Style::new().bg(theme::pal().sel_bg);
    let mut v = vec![Span::styled(" ", bg)];
    if ch.leader {
        v.push(Span::styled(
            "␣",
            bg.fg(theme::pal().net).add_modifier(Modifier::BOLD),
        ));
    }
    v.push(Span::styled(
        ch.text.clone(),
        bg.fg(c.key).add_modifier(Modifier::BOLD),
    ));
    v.push(Span::styled(" ", bg));
    v
}

/// The text with every occurrence of `q` highlighted.
fn marked(text: &str, style: Style, q: &str) -> Vec<Span<'static>> {
    if q.is_empty() {
        return vec![Span::styled(text.to_string(), style)];
    }
    let lower = text.to_lowercase();
    // Byte offsets only line up when lowercasing kept the lengths (ASCII and most text).
    if lower.len() != text.len() {
        return vec![Span::styled(text.to_string(), style)];
    }
    let hl = style
        .fg(theme::pal().warn)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = lower[at..].find(q) {
        let s = at + i;
        if s > at {
            out.push(Span::styled(text[at..s].to_string(), style));
        }
        out.push(Span::styled(text[s..s + q.len()].to_string(), hl));
        at = s + q.len();
    }
    if at < text.len() {
        out.push(Span::styled(text[at..].to_string(), style));
    }
    out
}

/// Chips, then the description wrapped under its own column; a trailing `(…)` dimmed.
fn key_row(r: &KeyRow, w: usize, c: &Colors, q: &str) -> Vec<Line<'static>> {
    let (start, lines) = help::key_row_lines(r, w);
    let desc_words = r.desc.split_whitespace().count();
    let mut word = 0;
    let mut out = Vec::new();
    for (i, text) in lines.iter().enumerate() {
        let mut spans: Vec<Span> = Vec::new();
        if i == 0 {
            for (n, ch) in r.chips.iter().enumerate() {
                if n > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.extend(chip_spans(ch, c));
            }
            let used = help::chips_width(&r.chips);
            spans.push(Span::raw(" ".repeat(start.saturating_sub(used))));
        } else {
            spans.push(Span::raw(" ".repeat(start)));
        }
        for (n, wd) in text.split(' ').enumerate() {
            if n > 0 {
                spans.push(Span::raw(" "));
            }
            let style = if word >= desc_words {
                theme::fg(theme::pal().dim)
            } else {
                theme::fg(theme::pal().fg)
            };
            spans.extend(marked(wd, style, q));
            word += 1;
        }
        out.push(Line::from(spans));
    }
    out
}

/// `:cd <path>   go to a folder …   g Space`: name purple, args teal italic, the key dim
/// at the right.
fn cmd_row(r: &CmdRow, w: usize, q: &str) -> Vec<Line<'static>> {
    let (start, lines) = help::cmd_row_lines(r, w);
    let hint = r.hint.as_ref().map(Chip::plain).unwrap_or_default();
    let hint = truncate(&hint, help::HINT_W + 1);
    let mut out = Vec::new();
    for (i, text) in lines.iter().enumerate() {
        let mut spans: Vec<Span> = Vec::new();
        let mut used = 0;
        if i == 0 {
            spans.extend(marked(&r.name, theme::bold(theme::pal().cmd), q));
            used += width(&r.name);
            if !r.args.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    r.args.clone(),
                    theme::fg(theme::pal().net).add_modifier(Modifier::ITALIC),
                ));
                used += 1 + width(&r.args);
            }
        }
        spans.push(Span::raw(" ".repeat(start.saturating_sub(used))));
        spans.extend(marked(text, theme::fg(theme::pal().fg), q));
        if i == 0 && !hint.is_empty() {
            let pad = w.saturating_sub(start + width(text) + width(&hint));
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(hint.clone(), theme::dim()));
        }
        out.push(Line::from(spans));
    }
    out
}
