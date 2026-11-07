//! The jobs panel (docs/JOBS_PANEL_SPEC.md): RUNNING cards and FINISHED entries on the
//! left; on the right the selected job's stage stepper, a wide progress bar, the
//! THROUGHPUT chart beside STATUS, PATHS and the ACTIVITY tail. A finished job shows its
//! report the same way. The keys live on the status bar's hint row.

use super::columns::panel;
use super::theme::{
    self, ACCENT, BORDER, DIM, DIR, ERR, FG, INACTIVE_SEL_BG, NET, OK, SEL_BG, WARN,
};
use super::{truncate, truncate_left, width};
use crate::app::{App, JobRow};
use crate::jobs::zip::BackupStep;
use crate::jobs::{ActKind, Activity, Job, Phase, Report, clock, files_word, human_bytes};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::time::{Duration, Instant, SystemTime};

/// The list pane's width (spec), and below what body width list and detail stack.
const LIST_W: u16 = 64;
const STACK_BELOW: u16 = 110;
/// Below this detail width THROUGHPUT and STATUS stack instead of sitting side by side.
const SIDE_BY_SIDE: usize = 84;
/// The progress bar's track and a running card's background.
const TRACK: Color = SEL_BG;
const CARD: Color = INACTIVE_SEL_BG;

pub fn draw(f: &mut Frame, body: Rect, app: &App) {
    f.render_widget(Clear, body);
    f.render_widget(
        ratatui::widgets::Block::new().style(Style::new().bg(theme::BG).fg(FG)),
        body,
    );
    let (list, detail, compact) = if body.width >= STACK_BELOW {
        let lw = LIST_W.min(body.width / 2);
        (
            Rect { width: lw, ..body },
            Rect {
                x: body.x + lw + 1,
                width: body.width.saturating_sub(lw + 1),
                ..body
            },
            false,
        )
    } else {
        // Stacked: the list takes what it needs (running cards, three finished), at most
        // half, so the detail keeps its room.
        let rows = app.job_rows();
        let running = app.jobs.iter().filter(|j| !j.phase.is_over()).count();
        let finished = rows.len().saturating_sub(running).min(3);
        let need = 2
            + if running > 0 { 1 + running * 4 } else { 0 }
            + if finished > 0 { 1 + finished * 2 } else { 0 }
            + usize::from(rows.is_empty()) * 2;
        let lh = (need as u16).clamp(5, (body.height / 2).max(5));
        (
            Rect { height: lh, ..body },
            Rect {
                y: body.y + lh,
                height: body.height.saturating_sub(lh),
                ..body
            },
            true,
        )
    };
    draw_list(f, list, app, compact);
    let now = Instant::now();
    match app.selected_job_row() {
        Some(JobRow::Live(i)) => match app.jobs.get(i) {
            Some(j) if !j.phase.is_over() => draw_live(f, detail, j, now),
            Some(j) => draw_report(f, detail, &j.report(now)),
            None => {}
        },
        Some(JobRow::Old(i)) => {
            if let Some(r) = app.old_reports.get(i) {
                draw_report(f, detail, r);
            }
        }
        None => {
            let block = panel(String::new(), false);
            let inner = block.inner(detail);
            f.render_widget(block, detail);
            f.render_widget(
                Paragraph::new(vec![
                    Line::default(),
                    Line::from(Span::styled(
                        "  no jobs: yy or x some files, then p in another folder",
                        theme::dim(),
                    )),
                ]),
                inner,
            );
        }
    }
}

// ------------------------------------------------------------------ colours, words

fn phase_color(p: Phase) -> Color {
    match p {
        Phase::Scanning | Phase::Copying | Phase::Finishing => ACCENT,
        Phase::Retrying => WARN,
        Phase::Paused => DIR,
        Phase::Done => OK,
        Phase::Failed => ERR,
        Phase::Cancelled => DIM,
    }
}

/// A running job's colour: paused blue, stalled yellow, else its phase's.
fn live_color(j: &Job, now: Instant) -> Color {
    if j.paused {
        DIR
    } else if j.stalled(now) {
        WARN
    } else {
        phase_color(j.phase)
    }
}

/// `COPYING`, `MOVING`, `ZIPPING 2/5`, `FINISHING`, `PAUSED`, `STALLED`.
fn status_word(j: &Job, now: Instant) -> String {
    if j.paused {
        return "PAUSED".to_string();
    }
    if j.stalled(now) {
        return "STALLED".to_string();
    }
    if let Some(step) = j.backup.as_ref().and_then(|b| b.step)
        && !j.phase.is_over()
    {
        return format!(
            "{} {}/{}",
            step.word().to_uppercase(),
            step.number(),
            BackupStep::ALL.len()
        );
    }
    match (j.phase, j.op) {
        (Phase::Copying, crate::jobs::Op::Move) => "MOVING".to_string(),
        (p, _) => p.word().to_string(),
    }
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

/// `12.4 MB/s`.
fn rate(bps: f64) -> String {
    format!("{}/s", human_bytes(bps.max(0.0) as u64))
}

/// A long name cut in the middle: `site-sur…ey.avi`.
fn middle(s: &str, w: usize) -> String {
    if width(s) <= w || w < 5 {
        return truncate(s, w);
    }
    let chars: Vec<char> = s.chars().collect();
    let keep = w - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

fn pad(n: usize) -> Span<'static> {
    Span::raw(" ".repeat(n))
}

fn hms(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%H:%M:%S")
        .to_string()
}

fn hm(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%H:%M")
        .to_string()
}

/// A smooth bar: full cells, one eighth-block cell, then the track.
fn bar(frac: f64, w: usize, color: Color) -> Vec<Span<'static>> {
    const PARTS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (frac.clamp(0.0, 1.0) * (w * 8) as f64).round() as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let mut v = vec![Span::styled("█".repeat(full), theme::fg(color))];
    let mut used = full;
    if part > 0 && used < w {
        v.push(Span::styled(PARTS[part], Style::new().fg(color).bg(TRACK)));
        used += 1;
    }
    v.push(Span::styled(
        "█".repeat(w.saturating_sub(used)),
        theme::fg(TRACK),
    ));
    v
}

/// `TITLE ──────────────── right`, as in `g?`.
fn rule(title: &str, right: &str, w: usize, color: Color) -> Line<'static> {
    let rw = if right.is_empty() {
        0
    } else {
        width(right) + 2
    };
    Line::from(vec![
        Span::styled(title.to_string(), theme::bold(color)),
        Span::raw(" "),
        Span::styled(
            "─".repeat(w.saturating_sub(width(title) + 1 + rw)),
            theme::fg(BORDER),
        ),
        Span::styled(
            if right.is_empty() {
                String::new()
            } else {
                format!(" {right} ")
            },
            theme::dim(),
        ),
    ])
}

/// A boxed notice (the stall banner, the pause note): a bold headline, what's going on
/// (wrapped), and the keys that help.
fn banner(
    color: Color,
    head: String,
    what: String,
    keys: &[(&str, &str)],
    w: usize,
) -> Vec<Line<'static>> {
    let inner = w.saturating_sub(4);
    let edge = |l: Vec<Span<'static>>| {
        let used: usize = l.iter().map(|s| s.width()).sum();
        let mut v = vec![Span::styled("│ ", theme::fg(color))];
        v.extend(l);
        v.push(pad(inner.saturating_sub(used)));
        v.push(Span::styled(" │", theme::fg(color)));
        Line::from(v)
    };
    let mut out = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(w.saturating_sub(2))),
        theme::fg(color),
    ))];
    out.push(edge(vec![Span::styled(
        truncate(&head, inner),
        theme::bold(color),
    )]));
    for l in crate::help::wrap(&what, inner) {
        out.push(edge(vec![Span::styled(l, theme::fg(FG))]));
    }
    let mut k: Vec<Span<'static>> = Vec::new();
    for (key, desc) in keys {
        k.push(Span::styled(key.to_string(), theme::bold(FG)));
        k.push(Span::styled(format!(" {desc}   "), theme::dim()));
    }
    out.push(edge(k));
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(w.saturating_sub(2))),
        theme::fg(color),
    )));
    out
}

/// Spans padded to `w` cells (so a background covers the whole line).
fn filled(mut spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let used: usize = spans.iter().map(|s| s.width()).sum();
    spans.push(pad(w.saturating_sub(used)));
    spans
}

// ------------------------------------------------------------------- the list

/// One finished entry's facts, from a live job's report or a kept one.
struct Finished {
    icon: (&'static str, Color),
    title: String,
    when: String,
    line2: String,
}

fn finished(r: &Report) -> Finished {
    // robocopy's "skipped" means already there and unchanged: not a warning.
    let icon = match r.phase {
        Phase::Failed => ("✗", ERR),
        Phase::Cancelled => ("–", DIM),
        // Done, but with files left out on purpose (`s`).
        _ if !r.skipped_files.is_empty() => ("!", WARN),
        _ => ("✓", OK),
    };
    let ended = r.started + Duration::from_secs(r.seconds);
    let mut what = match &r.backup {
        Some(b) => format!("zip {} · {} entries", human_bytes(b.bytes), b.entries),
        None => files_word(r.copied),
    };
    if !r.failed.is_empty() {
        what.push_str(&format!(" · {} failed", r.failed.len()));
    }
    if !r.skipped_files.is_empty() {
        what.push_str(&format!(" · {} skipped", r.skipped_files.len()));
    }
    if r.skipped > 0 && r.backup.is_none() {
        what.push_str(&format!(" · {} unchanged", r.skipped));
    }
    let (op, engine) = match &r.preset {
        Some(p) => (p.label.clone(), String::new()),
        None => (r.op.word().to_string(), format!(" ({})", r.backend.word())),
    };
    Finished {
        icon,
        title: r.title.clone(),
        when: hm(ended),
        line2: format!("{what} · {op}{engine} → {}", r.dest),
    }
}

fn draw_list(f: &mut Frame, area: Rect, app: &App, compact: bool) {
    let rows = app.job_rows();
    let block = panel("Jobs".into(), true)
        .title(Line::from(Span::styled(format!(" {} ", rows.len()), theme::dim())).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let now = Instant::now();
    let cursor = app.jobs_cursor.min(rows.len().saturating_sub(1));
    let running: Vec<(usize, &Job)> = rows
        .iter()
        .enumerate()
        .filter_map(|(n, r)| match *r {
            JobRow::Live(i) => app
                .jobs
                .get(i)
                .filter(|j| !j.phase.is_over())
                .map(|j| (n, j)),
            JobRow::Old(_) => None,
        })
        .collect();
    let done: Vec<(usize, Report)> = rows
        .iter()
        .enumerate()
        .filter_map(|(n, r)| match *r {
            JobRow::Live(i) => app
                .jobs
                .get(i)
                .filter(|j| j.phase.is_over())
                .map(|j| (n, j.report(now))),
            JobRow::Old(i) => app.old_reports.get(i).map(|r| (n, r.clone())),
        })
        .collect();

    // Every line, with the row it belongs to (to keep the selected one in view).
    let mut lines: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    if !running.is_empty() {
        lines.push((
            None,
            rule(&format!("RUNNING {}", running.len()), "", w, DIR),
        ));
        for (n, j) in &running {
            let sel = *n == cursor;
            let color = live_color(j, now);
            let mark = if sel {
                Span::styled("▌", Style::new().fg(color).bg(CARD))
            } else {
                Span::styled(" ", Style::new().bg(CARD))
            };
            let word = status_word(j, now);
            let title = middle(&j.title, w.saturating_sub(4 + width(&word)));
            let l1 = vec![
                mark.clone(),
                Span::styled(title.clone(), theme::bold(FG).bg(CARD)),
                pad(w.saturating_sub(2 + width(&title) + width(&word))),
                Span::styled(word, theme::bold(color).bg(CARD)),
            ];
            let pct = format!("{:.0}%", j.fraction() * 100.0);
            let right = match (&j.waiting, j.phase) {
                _ if j.paused => "paused".to_string(),
                (Some(wt), _) => format!(
                    "retry {} in {}",
                    wt.attempt,
                    clock(wt.next.saturating_duration_since(now))
                ),
                _ if j.stalled(now) => format!(
                    "no progress {}",
                    clock(now.duration_since(j.last_progress_at))
                ),
                (None, Phase::Scanning) => "scanning…".to_string(),
                (None, Phase::Finishing) => {
                    format!("finishing {}", clock(now.duration_since(j.phase_since)))
                }
                _ => j
                    .eta(now)
                    .map_or(String::new(), |e| format!("ETA {}", clock(e))),
            };
            let bw = w
                .saturating_sub(2 + 1 + width(&pct) + 2 + width(&right) + 1)
                .min(40);
            let mut l2 = vec![mark.clone(), Span::styled(" ", Style::new().bg(CARD))];
            l2.extend(bar(j.fraction(), bw, color));
            l2.push(Span::styled(format!(" {pct}"), theme::bold(FG).bg(CARD)));
            l2.push(Span::styled(format!("  {right}"), theme::dim().bg(CARD)));
            let mut facts = vec![kind_word(j.preset.as_ref(), j.op, j.backend)];
            if let Some(n) = j.total_files {
                facts.push(files_word(n));
            }
            if let Some(b) = j.total_bytes {
                facts.push(format!(
                    "{} / {}",
                    human_bytes(j.tracker.bytes_now()),
                    human_bytes(b)
                ));
            }
            if let Some(s) = j.smooth.filter(|s| *s > 0.0) {
                facts.push(rate(s));
            }
            let l3 = vec![
                mark,
                Span::styled(
                    format!(" {}", truncate(&facts.join(" · "), w.saturating_sub(2))),
                    theme::dim().bg(CARD),
                ),
            ];
            for l in [l1, l2, l3] {
                lines.push((
                    Some(*n),
                    Line::from(filled(l, w)).style(Style::new().bg(CARD)),
                ));
            }
            lines.push((None, Line::default()));
        }
    }
    let shown_done: Vec<&(usize, Report)> = if compact {
        done.iter().take(3).collect()
    } else {
        done.iter().collect()
    };
    if !done.is_empty() {
        lines.push((
            None,
            rule(&format!("FINISHED {}", done.len()), "↵ dismiss", w, DIR),
        ));
        for (n, r) in shown_done {
            let sel = *n == cursor;
            let e = finished(r);
            let bg = if sel { SEL_BG } else { theme::BG };
            let mark = if sel {
                Span::styled("▌", Style::new().fg(e.icon.1).bg(bg))
            } else {
                Span::styled(" ", Style::new().bg(bg))
            };
            let title = middle(&e.title, w.saturating_sub(5 + width(&e.when)));
            let l1 = vec![
                mark.clone(),
                Span::styled(e.icon.0, theme::bold(e.icon.1).bg(bg)),
                Span::styled(format!(" {title}"), theme::fg(FG).bg(bg)),
                pad(w.saturating_sub(3 + width(&title) + width(&e.when) + 1)),
                Span::styled(e.when, theme::dim().bg(bg)),
            ];
            let l2 = vec![
                mark,
                Span::styled(
                    format!("  {}", truncate(&e.line2, w.saturating_sub(4))),
                    theme::dim().bg(bg),
                ),
            ];
            for l in [l1, l2] {
                lines.push((
                    Some(*n),
                    Line::from(filled(l, w)).style(Style::new().bg(bg)),
                ));
            }
        }
    }
    if rows.is_empty() {
        lines.push((None, Line::default()));
        lines.push((
            None,
            Line::from(Span::styled("  no jobs yet", theme::dim())),
        ));
    }
    // Scroll so the selected entry is in view.
    let h = inner.height as usize;
    let last_sel = lines
        .iter()
        .rposition(|(r, _)| *r == Some(cursor))
        .unwrap_or(0);
    let first_sel = lines
        .iter()
        .position(|(r, _)| *r == Some(cursor))
        .unwrap_or(0);
    let top = if last_sel >= h {
        (last_sel + 1 - h).min(first_sel.saturating_sub(1))
    } else {
        0
    };
    let out: Vec<Line> = lines
        .into_iter()
        .skip(top)
        .take(h)
        .map(|(_, l)| l)
        .collect();
    f.render_widget(Paragraph::new(out), inner);
}

// ------------------------------------------------------------------- the detail

/// The stage stepper: `✓ Scanning 0:01 ──── ● Copying 0:32 ──── ○ Finishing ──── ○ Done`.
fn stepper(
    names: &[String],
    at: usize,
    times: &[Option<Duration>],
    current: Color,
    end: Option<(&'static str, Color)>,
    w: usize,
) -> Line<'static> {
    let mut parts: Vec<(String, Style)> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let t = times.get(i).copied().flatten().map(clock);
        let (mark, style) = if i < at {
            ("✓", theme::fg(OK))
        } else if i == at {
            match end {
                Some((m, c)) => (m, theme::bold(c)),
                None => ("●", theme::bold(current)),
            }
        } else {
            ("○", theme::dim())
        };
        let text = match t {
            Some(t) if i <= at => format!("{mark} {name} {t}"),
            _ => format!("{mark} {name}"),
        };
        parts.push((text, style));
    }
    let used: usize = parts.iter().map(|(t, _)| width(t)).sum();
    let gaps = parts.len().saturating_sub(1).max(1);
    let link = ((w.saturating_sub(used)) / gaps).clamp(3, 8);
    let mut spans = Vec::new();
    for (i, (t, s)) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                format!(" {} ", "─".repeat(link - 2)),
                theme::fg(BORDER),
            ));
        }
        spans.push(Span::styled(t, s));
    }
    Line::from(spans)
}

/// Stage names, where it is, and each stage's time, for a copy (robocopy / native).
fn copy_stages(
    times: &[(Phase, Duration)],
    phase: Phase,
) -> (Vec<String>, usize, Vec<Option<Duration>>) {
    let names = ["Scanning", "Copying", "Finishing", "Done"]
        .map(String::from)
        .to_vec();
    let at = match phase {
        Phase::Scanning => 0,
        Phase::Copying | Phase::Retrying => 1,
        Phase::Finishing => 2,
        _ => 3,
    };
    let get = |p: Phase| times.iter().find(|(q, _)| *q == p).map(|(_, d)| *d);
    let t = vec![
        get(Phase::Scanning),
        get(Phase::Copying),
        get(Phase::Finishing),
        None,
    ];
    (names, at, t)
}

fn backup_stages(b: &crate::jobs::zip::BackupState) -> (Vec<String>, usize) {
    let names = BackupStep::ALL
        .iter()
        .map(|s| {
            let w = s.word();
            let mut n = format!("{}{}", w[..1].to_uppercase(), &w[1..]);
            if *s == BackupStep::Staging && b.direct {
                n.push_str(" (skipped)");
            }
            n
        })
        .collect();
    (names, b.step.map_or(0, |s| s.number() - 1))
}

/// `16`, `50`, `200`: a round number at or above `v`.
fn nice_ceil(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        // A hair over a round number (float noise) still counts as it.
        if v <= m * p * 1.001 {
            return m * p;
        }
    }
    10.0 * p
}

/// THROUGHPUT: a 4-row block chart of the per-second samples (oldest left), y axis
/// from 0 to a round maximum, in MB/s (or KB/s for slow copies).
fn chart(samples: &[f64], w: usize, color: Color) -> Vec<Line<'static>> {
    const ROWS: usize = 4;
    const TICKS: [&str; 8] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];
    let peak = samples.iter().copied().fold(0.0, f64::max);
    // The same units as `human_bytes` (1 MB = 1024 × 1024).
    let (unit, div) = if peak >= 1_048_576.0 {
        ("MB/s", 1_048_576.0)
    } else {
        ("KB/s", 1_024.0)
    };
    let max = nice_ceil(peak / div);
    let top = if max >= 10.0 {
        format!("{max:.0}")
    } else {
        format!("{max}")
    };
    let lw = width(&top).max(1);
    let cols = w.saturating_sub(lw + 1).min(crate::jobs::HISTORY);
    let take = samples.len().min(cols);
    let shown = &samples[samples.len() - take..];
    let blank = cols - take;
    let mut out = Vec::new();
    for r in (0..ROWS).rev() {
        let label = if r == ROWS - 1 {
            format!("{top:>lw$}")
        } else if r == 0 {
            format!("{:>lw$}", "0")
        } else {
            " ".repeat(lw)
        };
        let mut cells = " ".repeat(blank);
        for v in shown {
            let e = ((v / div) / max * (ROWS * 8) as f64).round() as isize - (r * 8) as isize;
            cells.push_str(if e >= 8 {
                "█"
            } else if e > 0 {
                TICKS[e as usize]
            } else {
                " "
            });
        }
        out.push(Line::from(vec![
            Span::styled(label, theme::dim()),
            Span::styled("│", theme::fg(BORDER)),
            Span::styled(cells, theme::fg(color)),
        ]));
    }
    out.push(Line::from(vec![
        pad(lw),
        Span::styled(format!("└{}", "─".repeat(cols)), theme::fg(BORDER)),
    ]));
    let left = format!("−{} s", cols);
    let right = format!("{unit} · now");
    out.push(Line::from(vec![
        pad(lw + 1),
        Span::styled(left.clone(), theme::dim()),
        pad(cols.saturating_sub(width(&left) + width(&right))),
        Span::styled(right, theme::dim()),
    ]));
    out
}

/// A STATUS row: label (10 cols, dim), value (bold), note (dim).
fn stat(label: &str, value: String, color: Color, note: String) -> Line<'static> {
    let mut v = vec![
        Span::styled(format!("{label:<10}"), theme::dim()),
        Span::styled(value, theme::bold(color)),
    ];
    if !note.is_empty() {
        v.push(Span::styled(format!("  {note}"), theme::dim()));
    }
    Line::from(v)
}

/// Two blocks side by side (`a` then `b`, `gap` apart), or one under the other.
fn beside(
    a: Vec<Line<'static>>,
    aw: usize,
    b: Vec<Line<'static>>,
    side: bool,
) -> Vec<Line<'static>> {
    if !side {
        let mut v = a;
        v.push(Line::default());
        v.extend(b);
        return v;
    }
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| {
            let mut spans: Vec<Span> = a.get(i).map(|l| l.spans.clone()).unwrap_or_default();
            let used: usize = spans.iter().map(|s| s.width()).sum();
            spans.push(pad(aw.saturating_sub(used)));
            spans.push(Span::styled(" │ ", theme::fg(BORDER)));
            if let Some(l) = b.get(i) {
                spans.extend(l.spans.clone());
            }
            Line::from(spans)
        })
        .collect()
}

/// A labelled path, wrapped (never cut) under its column.
fn path_lines(label: &str, path: &str, color: Color, note: &str, w: usize) -> Vec<Line<'static>> {
    let avail = w.saturating_sub(8).max(10);
    let chars: Vec<char> = path.chars().collect();
    let mut out = Vec::new();
    for (i, chunk) in chars.chunks(avail).enumerate() {
        let mut v = vec![
            Span::styled(
                if i == 0 {
                    format!("{label:<8}")
                } else {
                    " ".repeat(8)
                },
                theme::dim(),
            ),
            Span::styled(chunk.iter().collect::<String>(), theme::fg(color)),
        ];
        if i == 0 && !note.is_empty() && chars.len() <= avail {
            v.push(Span::styled(format!("   {note}"), theme::dim()));
        }
        out.push(Line::from(v));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            format!("{label:<8}—"),
            theme::dim(),
        )));
    }
    out
}

fn act_color(k: ActKind) -> Color {
    match k {
        ActKind::Scan | ActKind::Done => OK,
        ActKind::Start | ActKind::Copy => DIR,
        ActKind::Retry(_) | ActKind::Skip => WARN,
        ActKind::Error => ERR,
    }
}

/// ACTIVITY: the newest `n` events, oldest first.
fn activity_lines(a: &[Activity], n: usize, w: usize) -> Vec<Line<'static>> {
    let from = a.len().saturating_sub(n);
    a[from..]
        .iter()
        .map(|e| {
            let k = e.kind.word();
            Line::from(vec![
                Span::styled(format!("{:<10}", hms(e.at)), theme::dim()),
                Span::styled(format!("{k:<9}"), theme::fg(act_color(e.kind))),
                Span::styled(truncate(&e.text, w.saturating_sub(19)), theme::fg(FG)),
            ])
        })
        .collect()
}

/// The detail pane's frame: the block and the padded area inside it.
fn detail_frame(f: &mut Frame, area: Rect, title: String, meta: String) -> Rect {
    let block = panel(title, false)
        .title(Line::from(Span::styled(format!(" {meta} "), theme::dim())).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    Rect {
        x: inner.x + 2,
        width: inner.width.saturating_sub(4),
        ..inner
    }
}

/// Sections top to bottom; ACTIVITY gets what's left (it shrinks first).
fn render_sections(
    f: &mut Frame,
    area: Rect,
    mut lines: Vec<Line<'static>>,
    activity: &[Activity],
) {
    let h = area.height as usize;
    let w = area.width as usize;
    let room = h.saturating_sub(lines.len() + 2);
    if room >= 1 && !activity.is_empty() {
        lines.push(Line::default());
        lines.push(rule("ACTIVITY", "tail of the log", w, DIR));
        lines.extend(activity_lines(activity, room.min(8), w));
    }
    lines.truncate(h);
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_live(f: &mut Frame, area: Rect, j: &Job, now: Instant) {
    let inner = detail_frame(
        f,
        area,
        j.title.clone(),
        kind_word(j.preset.as_ref(), j.op, j.backend),
    );
    let w = inner.width as usize;
    let color = live_color(j, now);
    let stalled = j.stalled(now);
    let mut lines = vec![Line::default()];

    // 1. The stepper: the current stage is `‖` paused, `◐` stalled, else `●`.
    let mark = if j.paused {
        Some(("‖", DIR))
    } else if stalled {
        Some(("◐", WARN))
    } else {
        None
    };
    lines.push(match &j.backup {
        Some(b) => {
            let (names, at) = backup_stages(b);
            stepper(&names, at, &[], color, mark, w)
        }
        None => {
            // A locked-file wait happens after the copying: it's part of Copying.
            let phase = if j.waiting.is_some() || j.paused {
                Phase::Copying
            } else {
                j.phase
            };
            let (mut names, at, mut times) = copy_stages(&j.stage_times(now), phase);
            if stalled && let Some(n) = names.get_mut(at) {
                // The stall time replaces the stage time there.
                if let Some(t) = times.get_mut(at) {
                    *t = None;
                }
                n.push_str(&format!(
                    " · stalled {}",
                    clock(now.duration_since(j.last_progress_at))
                ));
            }
            stepper(&names, at, &times, color, mark, w)
        }
    });

    // 2. Why it isn't moving: paused, or the stall banner.
    if j.paused {
        lines.push(Line::default());
        lines.extend(banner(
            DIR,
            "‖ Paused · p resumes".to_string(),
            "robocopy is stopped and holds no files; resuming carries on where it left off \
             (a half-copied file restarts from where it stopped)."
                .to_string(),
            &[("p", "resume"), ("c", "cancel job"), ("o", "open log")],
            w,
        ));
    } else if stalled {
        lines.push(Line::default());
        let quiet = clock(now.duration_since(j.last_progress_at));
        let (cause, what, keys): (String, String, &[(&str, &str)]) = match &j.waiting {
            Some(wt) => (
                "sentier is waiting to retry a locked file".to_string(),
                format!(
                    "{}: {} (error {}). Try {} of {} in {}. Everything else is copied.",
                    wt.path,
                    wt.message.trim().trim_end_matches('.'),
                    wt.code,
                    wt.attempt,
                    wt.max,
                    clock(wt.next.saturating_duration_since(now))
                ),
                &[
                    ("s", "skip this file"),
                    ("R", "retry now"),
                    ("c", "cancel job"),
                    ("o", "open log"),
                ],
            ),
            None if j.phase == Phase::Finishing => (
                "robocopy is finishing".to_string(),
                format!(
                    "Every byte is written; it is closing files on {} and hasn't exited yet.",
                    j.dest
                ),
                &[("c", "cancel job"), ("o", "open log")],
            ),
            None => (
                "no bytes moved; robocopy is still running".to_string(),
                match j.tracker.current.as_ref() {
                    Some(c) => format!("It's on {} ({:.0}%).", c.path, c.pct),
                    None => "It's between files (listing folders on a slow share?).".to_string(),
                },
                &[("p", "pause"), ("c", "cancel job"), ("o", "open log")],
            ),
        };
        lines.extend(banner(
            WARN,
            format!("⚠ No progress for {quiet} · {cause}"),
            what,
            keys,
            w,
        ));
    } else if j.phase == Phase::Finishing {
        lines.push(Line::default());
        let text = format!(
            "every byte is written; robocopy is closing files on {} and hasn't exited ({}).",
            j.dest,
            clock(now.duration_since(j.phase_since))
        );
        for (i, l) in crate::help::wrap(&text, w.saturating_sub(11))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(vec![
                if i == 0 {
                    Span::styled("Finishing: ", theme::bold(ACCENT))
                } else {
                    pad(11)
                },
                Span::styled(l, theme::fg(FG)),
            ]));
        }
    }

    // 2. The progress bar and the bytes.
    lines.push(Line::default());
    let frac = j.fraction();
    let pct = format!("{:>4.0}%", frac * 100.0);
    let mut b = bar(frac, w.saturating_sub(6), color);
    b.push(Span::styled(format!(" {pct}"), theme::bold(FG)));
    lines.push(Line::from(b));
    let later = j
        .backup
        .as_ref()
        .filter(|b| b.step.is_some_and(|s| s != BackupStep::Staging));
    let size = match (later, j.total_bytes) {
        (Some(b), _) => format!("{} of {}", human_bytes(b.done), human_bytes(b.total)),
        (None, Some(t)) => format!(
            "{} of {}",
            human_bytes(j.tracker.bytes_now()),
            human_bytes(t)
        ),
        (None, None) => "counting…".to_string(),
    };
    let (eta, eta_style) = match j.phase {
        _ if j.paused => ("ETA — paused".to_string(), theme::fg(DIR)),
        _ if j.waiting.is_some() => ("ETA waiting on a locked file".to_string(), theme::fg(WARN)),
        Phase::Scanning => ("ETA after the scan".to_string(), theme::dim()),
        Phase::Finishing => ("ETA — finishing".to_string(), theme::dim()),
        _ => (
            j.eta(now)
                .map_or("ETA —".to_string(), |e| format!("ETA {}", clock(e))),
            theme::dim(),
        ),
    };
    lines.push(Line::from(vec![
        Span::styled(size.clone(), theme::fg(FG)),
        pad(w.saturating_sub(width(&size) + width(&eta))),
        Span::styled(eta, eta_style),
    ]));
    if let Some(c) = j.tracker.current.as_ref().filter(|c| c.pct < 100.0)
        && later.is_none()
    {
        lines.push(Line::from(vec![
            Span::styled("now  ", theme::dim()),
            Span::styled(truncate_left(&c.path, w.saturating_sub(16)), theme::fg(FG)),
            Span::styled(format!("  {:.0}%", c.pct), theme::dim()),
        ]));
    }

    // 3. THROUGHPUT | STATUS.
    lines.push(Line::default());
    let side = w >= SIDE_BY_SIDE;
    let cw = if side { (w * 3 / 5).min(72) } else { w };
    let now_s = j.speed(now).or(j.smooth).unwrap_or(0.0);
    let avg = j.avg_speed(now).unwrap_or(0.0);
    let mut tp = vec![rule(
        "THROUGHPUT",
        &format!("{} now · {} avg", rate(now_s), rate(avg)),
        cw,
        DIR,
    )];
    let samples: Vec<f64> = j.history.iter().copied().collect();
    tp.extend(chart(&samples, cw, color));
    let t = &j.tracker;
    let files = format!(
        "{} / {}",
        t.files_done,
        j.total_files.map_or("?".to_string(), |n| n.to_string())
    );
    let errors = t.failed.len() + t.errors.len();
    let last_err = t
        .failed
        .last()
        .map(|e| short_reason(e.code))
        .or_else(|| t.errors.last().map(|e| short_reason(e.code)))
        .unwrap_or("");
    let last_retry = t
        .retries
        .last()
        .map(|r| format!("last: {} ({})", short_name(&r.path), short_reason(r.code)))
        .unwrap_or_default();
    let mut st = vec![rule("STATUS", "", if side { w - cw - 3 } else { w }, DIR)];
    st.push(stat("Speed", rate(now_s), FG, format!("avg {}", rate(avg))));
    st.push(stat("Files", files, FG, String::new()));
    st.push(stat(
        "Elapsed",
        clock(j.elapsed(now)),
        FG,
        format!("started {}", hms(j.started_at)),
    ));
    // sentier's own tries on locked files: the next one, and how often.
    let retry_note = match &j.waiting {
        Some(wt) => format!(
            "next {} · every {} s",
            clock(wt.next.saturating_duration_since(now)),
            crate::jobs::run::RETRY_EVERY.as_secs()
        ),
        None => last_retry,
    };
    st.push(stat(
        "Retries",
        j.retry_attempts.to_string(),
        if j.retry_attempts == 0 { FG } else { WARN },
        retry_note,
    ));
    if !j.skipped_files.is_empty() {
        st.push(stat(
            "Skipped",
            j.skipped_files.len().to_string(),
            WARN,
            "left out with s".to_string(),
        ));
    }
    st.push(if errors == 0 {
        stat("Errors", "none".into(), OK, String::new())
    } else {
        stat("Errors", errors.to_string(), ERR, last_err.to_string())
    });
    lines.extend(beside(tp, cw, st, side));

    // 4. PATHS.
    lines.push(Line::default());
    lines.push(rule("PATHS", "", w, DIR));
    lines.extend(path_lines("From", &j.source(), FG, "", w));
    lines.extend(path_lines("To", &j.dest, FG, "", w));
    lines.extend(path_lines("Log", &j.log, NET, "o open", w));

    let act: Vec<Activity> = j.activity.iter().cloned().collect();
    render_sections(f, inner, lines, &act);
}

fn draw_report(f: &mut Frame, area: Rect, r: &Report) {
    let inner = detail_frame(
        f,
        area,
        r.title.clone(),
        kind_word(r.preset.as_ref(), r.op, r.backend),
    );
    let w = inner.width as usize;
    let color = phase_color(r.phase);
    let mut lines = vec![Line::default()];
    let end = match r.phase {
        Phase::Failed => Some(("✗", ERR)),
        Phase::Cancelled => Some(("–", DIM)),
        _ => Some(("✓", OK)),
    };
    if r.backup.is_none() {
        let times: Vec<(Phase, Duration)> = r
            .stages
            .iter()
            .map(|(p, s)| (*p, Duration::from_secs(*s)))
            .collect();
        let (mut names, _, t) = copy_stages(&times, Phase::Done);
        if r.phase == Phase::Failed {
            names[3] = "Failed".into();
        } else if r.phase == Phase::Cancelled {
            names[3] = "Cancelled".into();
        }
        lines.push(stepper(&names, 3, &t, color, end, w));
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![Span::styled(
        truncate(&r.result_line(), w),
        theme::bold(color),
    )]));
    lines.push(Line::from(Span::styled(
        format!(
            "{} copied · {} unchanged · {} failed · {} extra  ·  {} in {}",
            r.copied,
            r.skipped,
            r.failed.len(),
            r.extra,
            human_bytes(r.bytes),
            clock(Duration::from_secs(r.seconds))
        ),
        theme::fg(FG),
    )));
    if let Some(b) = &r.backup {
        lines.push(Line::default());
        lines.extend(path_lines("Zip", &b.zip, NET, "", w));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<8}", "Verified"), theme::dim()),
            Span::styled(
                format!(
                    "{} entries read back, every CRC checked · {}",
                    b.entries,
                    human_bytes(b.bytes)
                ),
                theme::fg(OK),
            ),
        ]));
        let pruned = if b.pruned.is_empty() {
            "nothing (retention)".to_string()
        } else {
            b.pruned.join(", ")
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<8}", "Removed"), theme::dim()),
            Span::styled(truncate(&pruned, w.saturating_sub(8)), theme::fg(FG)),
        ]));
    }
    if !r.history.is_empty() {
        lines.push(Line::default());
        let samples: Vec<f64> = r.history.iter().map(|v| *v as f64).collect();
        let avg = if r.seconds > 0 {
            r.bytes as f64 / r.seconds as f64
        } else {
            0.0
        };
        let cw = w.min(72);
        lines.push(rule("THROUGHPUT", &format!("{} avg", rate(avg)), cw, DIR));
        lines.extend(chart(&samples, cw, color));
    }
    if !r.failed.is_empty() {
        lines.push(Line::default());
        lines.push(rule(
            &format!("FAILED {}", r.failed.len()),
            "r retries them",
            w,
            ERR,
        ));
        // The path, then why under it (wrapped: Windows' messages are long).
        for x in &r.failed {
            lines.push(Line::from(vec![
                Span::styled("✗ ", theme::fg(ERR)),
                Span::styled(truncate_left(&x.path, w.saturating_sub(2)), theme::fg(FG)),
            ]));
            let reason = format!("error {} · {}", x.code, x.message.trim());
            for l in crate::help::wrap(&reason, w.saturating_sub(2)) {
                lines.push(Line::from(vec![pad(2), Span::styled(l, theme::fg(ERR))]));
            }
        }
    }
    if !r.skipped_files.is_empty() {
        lines.push(Line::default());
        lines.push(rule(
            &format!("SKIPPED {}", r.skipped_files.len()),
            "left out with s (still locked)",
            w,
            WARN,
        ));
        for p in &r.skipped_files {
            lines.push(Line::from(vec![
                Span::styled("! ", theme::fg(WARN)),
                Span::styled(truncate_left(p, w.saturating_sub(2)), theme::fg(FG)),
            ]));
        }
    }
    if !r.errors.is_empty() {
        lines.push(Line::default());
        lines.push(rule("ERRORS", "", w, ERR));
        for e in &r.errors {
            lines.push(Line::from(Span::styled(truncate(e, w), theme::fg(ERR))));
        }
    }
    lines.push(Line::default());
    lines.push(rule("PATHS", "", w, DIR));
    if !r.source.is_empty() {
        lines.extend(path_lines("From", &r.source, FG, "", w));
    }
    lines.extend(path_lines("To", &r.dest, FG, "", w));
    lines.extend(path_lines("Log", &r.log, NET, "o open", w));
    render_sections(f, inner, lines, &r.activity);
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

fn short_name(path: &str) -> String {
    let name = path.rsplit('\\').next().unwrap_or(path);
    middle(name, 24)
}

// --------------------------------------------------------------- status bar parts

/// The status bar's line in the panel: `1 running · 3 done`, coloured.
pub fn summary_spans(app: &App) -> Vec<Span<'static>> {
    let running = app.jobs.iter().filter(|j| !j.phase.is_over()).count();
    let finished = app
        .jobs
        .iter()
        .filter(|j| j.phase.is_over())
        .map(|j| j.phase);
    let all: Vec<Phase> = finished
        .chain(app.old_reports.iter().map(|r| r.phase))
        .collect();
    let count = |p: Phase| all.iter().filter(|q| **q == p).count();
    let mut parts: Vec<(String, Color)> = Vec::new();
    if running > 0 {
        parts.push((format!("{running} running"), ACCENT));
    }
    for (p, word, c) in [
        (Phase::Done, "done", OK),
        (Phase::Failed, "failed", ERR),
        (Phase::Cancelled, "cancelled", DIM),
    ] {
        let n = count(p);
        if n > 0 {
            parts.push((format!("{n} {word}"), c));
        }
    }
    if parts.is_empty() {
        return vec![Span::raw("no jobs")];
    }
    let mut out = Vec::new();
    for (i, (t, c)) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::styled(" · ", theme::dim()));
        }
        out.push(Span::styled(t, theme::bold(c)));
    }
    out
}

/// The status bar's job indicator while jobs run: `⇅ 1 job · 38%`.
pub fn indicator(app: &App) -> Option<Span<'static>> {
    let running: Vec<&Job> = app.jobs.iter().filter(|j| !j.phase.is_over()).collect();
    if running.is_empty() {
        return None;
    }
    let now = Instant::now();
    let frac = running.iter().map(|j| j.fraction()).sum::<f64>() / running.len() as f64;
    let n = running.len();
    let jobs = if n == 1 { "job" } else { "jobs" };
    let pct = frac * 100.0;
    // A stall shows even with the panel closed (spec): that's what it's for.
    let stalled = running.iter().filter(|j| j.stalled(now)).count();
    let paused = running.iter().filter(|j| j.paused).count();
    let (text, color) = if stalled > 0 {
        (format!("⚠ {stalled} {jobs} stalled · {pct:.0}%   "), WARN)
    } else if paused == n {
        (format!("‖ {n} {jobs} paused · {pct:.0}%   "), DIR)
    } else {
        let finishing = running.iter().any(|j| j.phase == Phase::Finishing);
        (
            format!(
                "⇅ {n} {jobs} · {pct:.0}%{}   ",
                if finishing { " finishing" } else { "" }
            ),
            ACCENT,
        )
    };
    Some(Span::styled(
        text,
        Style::new()
            .fg(color)
            .add_modifier(Modifier::BOLD)
            .bg(SEL_BG),
    ))
}

/// The status bar's hint row in the panel, for the selected job: `p` always for a
/// running one, `s` / `R` while it waits on a locked file.
pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut h = vec![("jk", "pick job")];
    let live = match app.selected_job_row() {
        Some(JobRow::Live(i)) => app.jobs.get(i).filter(|j| !j.phase.is_over()),
        _ => None,
    };
    if let Some(j) = live {
        h.push(("c", "cancel"));
        h.push(("p", if j.paused { "resume" } else { "pause" }));
        if j.waiting.is_some() {
            h.push(("s", "skip locked file"));
            h.push(("R", "retry now"));
        }
    } else {
        h.push(("r", "retry failed"));
        h.push(("Enter", "dismiss"));
    }
    h.push(("o", "log"));
    h.push(("J", "close"));
    h
}
