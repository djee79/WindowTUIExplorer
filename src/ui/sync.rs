//! A sync preset's dry-run preview (docs/screens/05-sync-preview.txt): the folders, the
//! counts by kind of change, the list worth a look (deletions in mirror mode), and the
//! keys. Mirror's deletions are red; nothing here is touched until Enter.

use super::theme;
use super::{human_size, spinner, thousands, truncate, truncate_left, width};
use crate::app::{App, PreviewStatus};
use crate::jobs::preset::{Change, SyncMode};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

/// Lines below the list: blank, keys, blank, two notes.
const FOOTER: usize = 5;

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    let Some(sp) = app.sync_preview.as_ref() else {
        return;
    };
    let w = 104.min(body.width);
    let h = body.height.saturating_sub(2).max(10).min(body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height - h) / 2,
        width: w,
        height: h,
    };
    let mode_color = match sp.mode {
        SyncMode::Mirror => theme::pal().warn,
        SyncMode::Update => theme::pal().ok,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::fg(theme::pal().border))
        .style(Style::new().bg(theme::pal().panel_bg).fg(theme::pal().fg))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(theme::pal().border)),
            Span::styled("Sync preview · ", theme::dim()),
            Span::styled(sp.preset.name.clone(), theme::bold(theme::pal().accent)),
            Span::raw(" "),
        ]))
        .title(
            Line::from(Span::styled(
                format!(" {} ", sp.mode.word()),
                theme::bold(mode_color),
            ))
            .right_aligned(),
        );
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let iw = inner.width as usize;
    let vw = iw.saturating_sub(14);

    let field = |label: &str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("  {label:<10}"), theme::dim()),
            Span::styled(value, style),
        ])
    };
    let mut lines = vec![
        Line::default(),
        field(
            "Source",
            truncate_left(&sp.preset.source.display(), vw),
            theme::fg(theme::pal().net),
        ),
        field(
            "Dest",
            truncate_left(&sp.preset.dest.display(), vw),
            theme::fg(theme::pal().fg),
        ),
    ];
    let mut excl = Vec::new();
    if !sp.preset.exclude_dirs.is_empty() {
        excl.push(format!("/XD {}", sp.preset.exclude_dirs.join(" ")));
    }
    if !sp.preset.exclude_files.is_empty() {
        excl.push(format!("/XF {}", sp.preset.exclude_files.join(" ")));
    }
    if !excl.is_empty() {
        lines.push(field(
            "Excludes",
            truncate(&excl.join(" · "), vw),
            theme::dim(),
        ));
    }
    let now = std::time::Instant::now();
    let took = sp.took.unwrap_or_else(|| now.duration_since(sp.started));
    let run = match &sp.status {
        PreviewStatus::Running => format!(
            "robocopy /L /MIR  {} running… {:.1} s",
            spinner(app),
            took.as_secs_f64()
        ),
        _ => format!("robocopy /L /MIR  ·  {:.1} s", took.as_secs_f64()),
    };
    lines.push(field("Dry run", run, theme::fg(theme::pal().fg)));
    lines.push(Line::default());

    let keys: Vec<(&str, String)> = match &sp.status {
        PreviewStatus::Running => {
            lines.push(Line::from(Span::styled(
                "  Listing what would change. Nothing is copied or deleted.",
                theme::dim(),
            )));
            vec![("Esc", "Cancel".into())]
        }
        PreviewStatus::Failed(e) => {
            lines.push(Line::from(Span::styled(
                "  The dry run failed, so the sync can't run:",
                theme::bold(theme::pal().err),
            )));
            for chunk in wrap(e, iw.saturating_sub(6)) {
                lines.push(Line::from(Span::styled(
                    format!("    {chunk}"),
                    theme::fg(theme::pal().err),
                )));
            }
            vec![("R", "Try again".into()), ("Esc", "Close".into())]
        }
        PreviewStatus::Ready(pv) => {
            let count = |n: u64, what: &str, bytes: Option<u64>, style: Style| {
                let left = format!("  {:>7}  {what}", thousands(n));
                let right = bytes.map(human_size).unwrap_or_default();
                let pad = iw.saturating_sub(width(&left) + width(&right) + 3);
                Line::from(vec![
                    Span::styled(left, style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(right, style),
                ])
            };
            let (n, b) = pv.to_copy();
            let (r, rb) = pv.count(Change::Replace);
            let (x, xb) = pv.count(Change::Extra);
            lines.push(count(
                n,
                "files to copy",
                Some(b),
                theme::bold(theme::pal().fg),
            ));
            if r > 0 {
                lines.push(count(
                    r,
                    "of them replace a local copy (changed on the server)",
                    Some(rb),
                    theme::fg(theme::pal().fg),
                ));
            }
            if x > 0 {
                lines.push(match sp.mode {
                    SyncMode::Mirror => count(
                        x,
                        "local files will be DELETED",
                        Some(xb),
                        theme::bold(theme::pal().err),
                    ),
                    SyncMode::Update => count(
                        x,
                        "local-only files, kept (update never deletes)",
                        Some(xb),
                        theme::dim(),
                    ),
                });
            }
            if sp.mode == SyncMode::Mirror && !pv.extra_dirs.is_empty() {
                lines.push(count(
                    pv.extra_dirs.len() as u64,
                    "local folders will be DELETED",
                    None,
                    theme::bold(theme::pal().err),
                ));
            }
            lines.push(count(
                pv.unchanged,
                "unchanged, skipped",
                None,
                theme::dim(),
            ));
            lines.push(Line::default());

            let (title, rows) = sp.listed();
            if pv.is_noop(sp.mode) {
                lines.push(Line::from(Span::styled(
                    "  Nothing to do: the local copy is up to date.",
                    theme::bold(theme::pal().ok),
                )));
            } else if !rows.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {title}"),
                    theme::bold(theme::pal().fg),
                )));
                lines.push(Line::from(Span::styled(
                    format!("  {}", "─".repeat(iw.saturating_sub(4))),
                    theme::fg(theme::pal().border),
                )));
                let room = (inner.height as usize).saturating_sub(lines.len() + FOOTER);
                // `scroll` is the cursor (x / X act on that row); the window follows it.
                let cursor = sp.scroll.min(rows.len().saturating_sub(1));
                let start = (cursor + 1).saturating_sub(room.max(1));
                for (i, row) in rows.iter().enumerate().skip(start).take(room) {
                    let style = match (row.change, sp.mode) {
                        (Change::Extra, SyncMode::Mirror) => theme::fg(theme::pal().err),
                        (Change::Extra, SyncMode::Update) => theme::dim(),
                        (Change::Replace, _) => theme::fg(theme::pal().warn),
                        (Change::New, _) => theme::fg(theme::pal().fg),
                    };
                    let size = human_size(row.size);
                    let pw = iw.saturating_sub(width(&size) + 8);
                    let path = truncate_left(&row.rel, pw);
                    let pad = iw.saturating_sub(4 + width(&path) + width(&size) + 3);
                    let line = Line::from(vec![
                        Span::styled(format!("  {} ", row.change.sign()), style),
                        Span::styled(path, style),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(size, theme::fg(theme::pal().dim)),
                    ]);
                    lines.push(if i == cursor {
                        line.style(ratatui::style::Style::new().bg(theme::pal().sel_bg))
                    } else {
                        line
                    });
                }
                if rows.len() > room {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  {}–{} of {}  (j/k ^d/^u move · x leave this file out · X its folder)",
                            start + 1,
                            (start + room).min(rows.len()),
                            rows.len()
                        ),
                        theme::dim(),
                    )));
                }
            }
            let run = if pv.is_noop(sp.mode) {
                "Close".to_string()
            } else {
                format!("Run {}", sp.mode.word())
            };
            let mut keys = vec![
                ("Enter", run),
                ("u", format!("Switch to {}", sp.mode.flip().word())),
                (
                    "Tab",
                    if sp.show_all {
                        "Short list".to_string()
                    } else {
                        format!("Show all {}", pv.rows.len())
                    },
                ),
                ("x / X", "Leave out file / folder".into()),
            ];
            // What `x` / `X` (or pins.toml) left out, and how to bring it back.
            let left_out = sp.preset.exclude_dirs.len() + sp.preset.exclude_files.len();
            if left_out > 0 {
                keys.push(("E", format!("Bring back {left_out} left out")));
            }
            keys.push(("Esc", "Cancel".into()));
            keys
        }
    };

    // The footer sits at the bottom of the panel.
    let footer_at = (inner.height as usize).saturating_sub(FOOTER);
    while lines.len() < footer_at {
        lines.push(Line::default());
    }
    lines.truncate(footer_at);
    lines.push(Line::default());
    // The keys, on a second line when they don't fit on one (it takes the blank's place).
    let mut rows: Vec<Vec<Span>> = vec![vec![Span::raw("  ")]];
    let mut used = 2;
    for (k, what) in keys {
        let w = width(k) + width(&what) + 8;
        if used + w > iw && rows.len() < 2 && used > 2 {
            rows.push(vec![Span::raw("  ")]);
            used = 2;
        }
        used += w;
        let Some(spans) = rows.last_mut() else {
            break;
        };
        spans.push(Span::styled("[ ", theme::fg(theme::pal().border)));
        spans.push(Span::styled(k, theme::bold(theme::pal().accent)));
        spans.push(Span::styled(format!(" {what}"), theme::fg(theme::pal().fg)));
        spans.push(Span::styled(" ]   ", theme::fg(theme::pal().border)));
    }
    let two = rows.len() > 1;
    for spans in rows {
        lines.push(Line::from(spans));
    }
    if !two {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        "  Mirror always previews first. A preset can't push local → server.",
        theme::dim(),
    )));
    lines.push(Line::from(Span::styled(
        "  Update mode only adds and overwrites; it never deletes local files.",
        theme::dim(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

/// Cuts `s` into pieces of at most `w` characters.
fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(10);
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}
