//! The jobs panel (docs/screens/04-jobs.txt): the jobs on the left, newest first; the
//! selected one on the right. A running job shows its phases, the current file, bytes,
//! files, speed, retries and errors; a finished one its report.

use super::columns::panel;
use super::theme::{self, ACCENT, BORDER, DIM, ERR, FG, NET, OK, SEL_BG, WARN};
use super::{truncate, truncate_left, width};
use crate::app::{App, JobRow};
use crate::jobs::{Job, Phase, Report, clock, human_bytes};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::time::Instant;

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    f.render_widget(Clear, body);
    f.render_widget(
        ratatui::widgets::Block::new().style(Style::new().bg(theme::BG).fg(FG)),
        body,
    );
    let [left, right] =
        Layout::horizontal([Constraint::Fill(36), Constraint::Fill(64)]).areas(body);
    draw_list(f, left, app);
    match app.selected_job_row() {
        Some(JobRow::Live(i)) => match app.jobs.get(i) {
            Some(j) if !j.phase.is_over() => draw_detail(f, right, j),
            Some(j) => draw_report(f, right, &j.report(Instant::now())),
            None => {}
        },
        Some(JobRow::Old(i)) => {
            if let Some(r) = app.old_reports.get(i) {
                draw_report(f, right, r);
            }
        }
        None => {
            let block = panel(String::new(), false);
            let inner = block.inner(right);
            f.render_widget(block, right);
            f.render_widget(
                Paragraph::new(vec![
                    Line::default(),
                    Line::from(Span::styled(
                        " no jobs: yy or x some files, then p in another folder",
                        theme::dim(),
                    )),
                ]),
                inner,
            );
        }
    }
}

fn phase_color(p: Phase) -> ratatui::style::Color {
    match p {
        Phase::Scanning | Phase::Copying => ACCENT,
        Phase::Retrying | Phase::Finishing => WARN,
        Phase::Done => OK,
        Phase::Failed => ERR,
        Phase::Cancelled => DIM,
    }
}

/// `██████░░░░`
fn bar(frac: f64, w: usize) -> (String, String) {
    let done = ((frac.clamp(0.0, 1.0)) * w as f64).round() as usize;
    ("█".repeat(done), "░".repeat(w.saturating_sub(done)))
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.job_rows();
    let block = panel("Jobs".into(), true)
        .title(Line::from(Span::styled(format!(" {} ", rows.len()), theme::dim())).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let now = Instant::now();
    let mut lines = vec![Line::default()];
    let cursor = app.jobs_cursor.min(rows.len().saturating_sub(1));
    for (n, row) in rows.iter().enumerate() {
        let sel = n == cursor;
        let mark = if sel {
            Span::styled("▌ ", theme::bold(ACCENT))
        } else {
            Span::raw("  ")
        };
        let (title, phase, line2, line3) = match *row {
            JobRow::Live(i) => {
                let Some(j) = app.jobs.get(i) else { continue };
                if j.phase.is_over() {
                    let r = j.report(now);
                    (r.title.clone(), r.phase, report_counts(&r), report_kind(&r))
                } else {
                    (
                        j.title.clone(),
                        j.phase,
                        progress_line(j, w, now),
                        job_kind(j),
                    )
                }
            }
            JobRow::Old(i) => {
                let Some(r) = app.old_reports.get(i) else {
                    continue;
                };
                (r.title.clone(), r.phase, report_counts(r), report_kind(r))
            }
        };
        let word = phase.word();
        let tw = w.saturating_sub(3 + width(word) + 2);
        let t = truncate(&title, tw);
        let pad = w.saturating_sub(2 + width(&t) + width(word) + 1);
        let title_style = if sel { theme::bold(FG) } else { theme::fg(FG) };
        lines.push(Line::from(vec![
            mark,
            Span::styled(t, title_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(word, theme::bold(phase_color(phase))),
        ]));
        lines.push(line2);
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&line3, w.saturating_sub(3))),
            theme::dim(),
        )));
        lines.push(Line::default());
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled("  no jobs yet", theme::dim())));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// `  ███████░░░  38%        ETA 4:10` or, finishing, `0:47 in phase`.
fn progress_line(j: &Job, w: usize, now: Instant) -> Line<'static> {
    let right = match j.phase {
        Phase::Finishing | Phase::Retrying => {
            format!("{} in phase", clock(now.duration_since(j.phase_since)))
        }
        Phase::Scanning => "scanning…".to_string(),
        _ => j
            .eta(now)
            .map_or(String::new(), |e| format!("ETA {}", clock(e))),
    };
    let pct = format!("{:>3.0}%", j.fraction() * 100.0);
    let bw = w
        .saturating_sub(2 + 1 + width(&pct) + 2 + width(&right) + 1)
        .min(30);
    let (done, left) = bar(j.fraction(), bw);
    let pad = w.saturating_sub(2 + bw + 1 + width(&pct) + width(&right) + 1);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(done, theme::fg(phase_color(j.phase))),
        Span::styled(left, theme::fg(BORDER)),
        Span::raw(" "),
        Span::styled(pct, theme::fg(FG)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, theme::dim()),
    ])
}

fn job_kind(j: &Job) -> String {
    let mut s = kind_word(j.preset.as_ref(), j.op, j.backend);
    if let (Some(f), Some(b)) = (j.total_files, j.total_bytes) {
        s.push_str(&format!(" · {f} files · {}", human_bytes(b)));
    }
    s
}

fn report_kind(r: &Report) -> String {
    format!(
        "{} → {}",
        kind_word(r.preset.as_ref(), r.op, r.backend),
        r.dest
    )
}

/// `sync · mirror` for a preset's job, else `copy · robocopy`.
fn kind_word(
    preset: Option<&crate::jobs::preset::PresetRef>,
    op: crate::jobs::Op,
    backend: crate::jobs::Backend,
) -> String {
    match preset {
        Some(p) => p.label.clone(),
        None => format!("{} · {}", op.word(), backend.word()),
    }
}

/// `  2 copied · 1 failed                      19:31`
fn report_counts(r: &Report) -> Line<'static> {
    let mut parts = vec![format!("{} copied", r.copied)];
    if r.skipped > 0 {
        parts.push(format!("{} skipped", r.skipped));
    }
    if !r.failed.is_empty() {
        parts.push(format!("{} failed", r.failed.len()));
    }
    let when = chrono::DateTime::<chrono::Local>::from(r.started)
        .format("%H:%M")
        .to_string();
    Line::from(vec![
        Span::styled(format!("  {}", parts.join(" · ")), theme::fg(FG)),
        Span::styled(format!("   {when}"), theme::dim()),
    ])
}

fn label(text: &str) -> Span<'static> {
    Span::styled(format!(" {text:<10} "), theme::dim())
}

fn draw_detail(f: &mut Frame, area: Rect, j: &Job) {
    let now = Instant::now();
    let block = panel(j.title.clone(), false).title(
        Line::from(Span::styled(
            format!(" {} ", kind_word(j.preset.as_ref(), j.op, j.backend)),
            theme::dim(),
        ))
        .right_aligned(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    let vw = (inner.width as usize).saturating_sub(13);
    let mut lines = vec![Line::default()];

    // ● Scanning ── ● Copying ── ● Finishing ── ○ Done
    let order = [
        Phase::Scanning,
        Phase::Copying,
        Phase::Finishing,
        Phase::Done,
    ];
    let at = match j.phase {
        Phase::Scanning => 0,
        Phase::Copying | Phase::Retrying => 1,
        Phase::Finishing => 2,
        _ => 3,
    };
    let mut chain = vec![Span::raw(" ")];
    for (i, p) in order.iter().enumerate() {
        if i > 0 {
            chain.push(Span::styled("  ──  ", theme::fg(BORDER)));
        }
        let name = match p {
            Phase::Scanning => "Scanning",
            Phase::Copying if j.phase == Phase::Retrying => "Retrying",
            Phase::Copying => "Copying",
            Phase::Finishing => "Finishing",
            _ => "Done",
        };
        let (dot, style) = if i < at {
            ("● ", theme::fg(OK))
        } else if i == at {
            ("● ", theme::bold(phase_color(j.phase)))
        } else {
            ("○ ", theme::dim())
        };
        chain.push(Span::styled(format!("{dot}{name}"), style));
    }
    lines.push(Line::from(chain));
    lines.push(Line::default());

    if j.phase == Phase::Finishing {
        lines.push(Line::from(vec![
            Span::styled(" Finishing: ", theme::bold(WARN)),
            Span::styled(
                format!(
                    "every byte is written. robocopy is closing files on {} and hasn't exited yet.",
                    truncate_left(&j.dest, 40)
                ),
                theme::fg(FG),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "            This is the old \"stuck at 99%\", now named and timed.",
            theme::dim(),
        )));
        lines.push(Line::default());
    }
    if j.phase == Phase::Scanning {
        lines.push(Line::from(Span::styled(
            " Scanning: robocopy /L is listing what there is to copy…",
            theme::dim(),
        )));
        lines.push(Line::default());
    }

    let t = &j.tracker;
    let current = match &t.current {
        Some(c) if c.pct < 100.0 => format!(
            "{}  ({}) · {:.0}%{}",
            truncate_left(&c.path, vw.saturating_sub(24)),
            human_bytes(c.size),
            c.pct,
            if c.attempt > 1 {
                format!(" · attempt {}", c.attempt)
            } else {
                String::new()
            }
        ),
        Some(c) if j.phase == Phase::Finishing => format!(
            "{}  ({}) · closing",
            truncate_left(&c.path, vw.saturating_sub(24)),
            human_bytes(c.size)
        ),
        _ => "—".to_string(),
    };
    lines.push(Line::from(vec![
        label("Current"),
        Span::styled(current, theme::fg(FG)),
    ]));
    let total_b = j.total_bytes.map_or("?".into(), human_bytes);
    let total_f = j.total_files.map_or("?".into(), |n| n.to_string());
    lines.push(Line::from(vec![
        label("Bytes"),
        Span::styled(
            format!(
                "{:<20}",
                format!("{} / {total_b}", human_bytes(t.bytes_now()))
            ),
            theme::fg(FG),
        ),
        Span::styled("  Files    ", theme::dim()),
        Span::styled(format!("{} / {total_f}", t.files_done), theme::fg(FG)),
    ]));
    let speed = match (j.speed(now), j.last_speed) {
        (Some(s), _) if s > 0.0 => format!("{}/s", human_bytes(s as u64)),
        (_, Some(last)) => format!("—  (last {}/s)", human_bytes(last as u64)),
        _ => "—".to_string(),
    };
    let elapsed = if j.phase == Phase::Finishing {
        format!(
            "{}  ({} finishing)",
            clock(j.elapsed(now)),
            clock(now.duration_since(j.phase_since))
        )
    } else {
        clock(j.elapsed(now))
    };
    lines.push(Line::from(vec![
        label("Speed"),
        Span::styled(format!("{speed:<20}"), theme::fg(FG)),
        Span::styled("  Elapsed  ", theme::dim()),
        Span::styled(elapsed, theme::fg(FG)),
    ]));
    lines.push(Line::default());

    lines.push(Line::from(vec![
        label("Retries"),
        Span::styled(t.retries.len().to_string(), theme::fg(FG)),
    ]));
    for r in t.retries.iter().rev().take(4) {
        let how = match r.ok {
            Some(true) => format!("→  ok on attempt {}", r.attempts),
            Some(false) => "→  gave up".to_string(),
            None => format!("attempt {}…", r.attempts + 1),
        };
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(12)),
            Span::styled(truncate_left(&r.path, vw.saturating_sub(40)), theme::fg(FG)),
            Span::styled(
                format!("   {} (ERROR {})  ", short_reason(r.code), r.code),
                theme::fg(WARN),
            ),
            Span::styled(how, theme::dim()),
        ]));
    }
    let errs: Vec<String> = t
        .failed
        .iter()
        .map(|e| format!("{}  ERROR {} {}", e.path, e.code, e.message))
        .chain(t.errors.iter().map(|e| {
            format!(
                "ERROR {} {} {}  {}",
                e.code,
                e.action,
                e.path.as_deref().unwrap_or(""),
                e.message
            )
        }))
        .collect();
    lines.push(Line::from(vec![
        label("Errors"),
        Span::styled(
            if errs.is_empty() {
                "none".to_string()
            } else {
                errs.len().to_string()
            },
            theme::fg(if errs.is_empty() { FG } else { ERR }),
        ),
    ]));
    for e in errs.iter().rev().take(4) {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(12)),
            Span::styled(truncate_left(e, vw), theme::fg(ERR)),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        label("Log"),
        Span::styled(truncate_left(&j.log, vw), theme::fg(NET)),
    ]));
    footer(
        f,
        inner,
        lines,
        &[("c", "cancel"), ("o", "open log"), ("J", "hide panel")],
    );
}

/// `locked`, `access denied`, …
fn short_reason(code: u32) -> &'static str {
    match code {
        32 | 33 => "locked",
        5 => "access denied",
        2 | 3 => "not found",
        53 | 64 | 67 => "network",
        112 => "disk full",
        _ => "error",
    }
}

fn draw_report(f: &mut Frame, area: Rect, r: &Report) {
    let when = chrono::DateTime::<chrono::Local>::from(r.started)
        .format("%H:%M")
        .to_string();
    let block = panel("Report".into(), false).title(
        Line::from(Span::styled(
            format!(" {} · {when} ", r.title),
            theme::dim(),
        ))
        .right_aligned(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    let vw = (inner.width as usize).saturating_sub(13);
    let mut lines = vec![Line::default()];
    lines.push(Line::from(vec![
        label("Result"),
        Span::styled(
            truncate(&r.result_line(), vw),
            theme::bold(phase_color(r.phase)),
        ),
    ]));
    lines.push(Line::from(vec![
        label("Counts"),
        Span::styled(
            format!(
                "copied {}  ·  skipped {}  ·  failed {}  ·  extra {}",
                r.copied,
                r.skipped,
                r.failed.len(),
                r.extra
            ),
            theme::fg(FG),
        ),
    ]));
    lines.push(Line::from(vec![
        label("Size"),
        Span::styled(
            format!(
                "{} in {}",
                human_bytes(r.bytes),
                clock(std::time::Duration::from_secs(r.seconds))
            ),
            theme::fg(FG),
        ),
    ]));
    lines.push(Line::from(vec![
        label("Into"),
        Span::styled(truncate_left(&r.dest, vw), theme::fg(FG)),
    ]));
    if !r.retried.is_empty() {
        lines.push(Line::from(vec![
            label("Retried"),
            Span::styled(
                format!("{} file(s) worked on a later attempt", r.retried.len()),
                theme::fg(FG),
            ),
        ]));
    }
    lines.push(Line::default());
    if !r.failed.is_empty() {
        lines.push(Line::from(Span::styled(" Failed files", theme::bold(FG))));
        lines.push(Line::from(Span::styled(
            format!(" {}", "─".repeat((inner.width as usize).saturating_sub(2))),
            theme::fg(BORDER),
        )));
        for x in &r.failed {
            let reason = format!("ERROR {} {}", x.code, x.message);
            let pw = (inner.width as usize)
                .saturating_sub(width(&reason) + 6)
                .max(20);
            lines.push(Line::from(vec![
                Span::styled(" × ", theme::fg(ERR)),
                Span::styled(
                    format!("{:<pw$}", truncate_left(&x.path, pw)),
                    theme::fg(FG),
                ),
                Span::styled(format!("  {reason}"), theme::fg(ERR)),
            ]));
        }
        lines.push(Line::default());
    }
    if !r.errors.is_empty() {
        lines.push(Line::from(Span::styled(" Errors", theme::bold(FG))));
        for e in &r.errors {
            lines.push(Line::from(Span::styled(
                format!(" {}", truncate(e, (inner.width as usize).saturating_sub(2))),
                theme::fg(ERR),
            )));
        }
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![
        label("Log"),
        Span::styled(truncate_left(&r.log, vw), theme::fg(NET)),
    ]));
    let keys: &[(&str, &str)] = if r.failed.is_empty() {
        &[("o", "open log"), ("Enter", "dismiss")]
    } else {
        &[
            ("r", "retry failed only"),
            ("o", "open log"),
            ("Enter", "dismiss"),
        ]
    };
    footer(f, inner, lines, keys);
}

/// The lines, then the keys on the last row.
fn footer(f: &mut Frame, inner: Rect, mut lines: Vec<Line<'static>>, keys: &[(&str, &str)]) {
    let h = inner.height as usize;
    lines.truncate(h.saturating_sub(2));
    while lines.len() < h.saturating_sub(1) {
        lines.push(Line::default());
    }
    let mut spans = vec![Span::raw(" ")];
    for (k, what) in keys {
        spans.push(Span::styled(k.to_string(), theme::bold(FG)));
        spans.push(Span::styled(format!(" {what}   "), theme::dim()));
    }
    lines.push(Line::from(spans));
    f.render_widget(Paragraph::new(lines), inner);
}

/// The header's summary: `1 finishing · 1 running · 1 failed`.
pub fn summary(app: &App) -> String {
    let mut counts: Vec<(Phase, usize)> = Vec::new();
    let phases = app
        .jobs
        .iter()
        .map(|j| j.phase)
        .chain(app.old_reports.iter().map(|r| r.phase));
    for p in phases {
        let p = match p {
            Phase::Scanning | Phase::Copying | Phase::Retrying => Phase::Copying,
            other => other,
        };
        match counts.iter_mut().find(|(q, _)| *q == p) {
            Some((_, n)) => *n += 1,
            None => counts.push((p, 1)),
        }
    }
    counts
        .iter()
        .map(|(p, n)| {
            let w = match p {
                Phase::Copying => "running",
                Phase::Finishing => "finishing",
                Phase::Done => "done",
                Phase::Failed => "failed",
                Phase::Cancelled => "cancelled",
                Phase::Retrying | Phase::Scanning => "running",
            };
            format!("{n} {w}")
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The status bar's job indicator while jobs run: `⇅ 1 job 38%`.
pub fn indicator(app: &App) -> Option<Span<'static>> {
    let running: Vec<&Job> = app.jobs.iter().filter(|j| !j.phase.is_over()).collect();
    if running.is_empty() {
        return None;
    }
    let frac = running.iter().map(|j| j.fraction()).sum::<f64>() / running.len() as f64;
    let n = running.len();
    let any_finishing = running.iter().any(|j| j.phase == Phase::Finishing);
    let text = format!(
        "⇅ {n} job{} {:.0}%{}   ",
        if n == 1 { "" } else { "s" },
        frac * 100.0,
        if any_finishing { " finishing" } else { "" }
    );
    Some(Span::styled(
        text,
        Style::new()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD)
            .bg(SEL_BG),
    ))
}
