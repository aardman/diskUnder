//! Drawing the app: menu bar, main window (list and details), run strip, footer, views,
//! menus and dialogs

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table},
    Frame,
};

use super::results::{Browse, Dups, Fields, Search};
use super::{App, Dialog, InputFor, PathEntry, Selected, View, MENUS, REPORTS, TEST_STEPS};
use crate::cli::display_version;
use crate::fmt::{fmt_bytes, fmt_count, fmt_local, truncate_path};
use crate::jobs::{fmt_secs, opt_count, status, JobSummary, RunRow};

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

fn status_style(text: &str) -> Style {
    match text {
        "completed" => Style::new().fg(Color::Green),
        "running" => Style::new().fg(Color::Yellow),
        "never" | "—" => DIM,
        s if s.starts_with("failed") || s == "did not finish" => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::Yellow),
    }
}

/// "✓ 02:14 today" style one-word marker for a run
fn mark(r: &RunRow, running: bool) -> (&'static str, Style) {
    let s = status(r, running);
    let symbol = match s.as_str() {
        "completed" => "✓",
        "running" => "●",
        "stopped by user" => "■",
        _ => "✗",
    };
    (symbol, status_style(&s))
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let strip = if app.running.is_some() || app.finished.is_some() { 3 } else { 0 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(strip),
            Constraint::Length(2),
        ])
        .split(f.area());
    draw_menu_bar(f, app, rows[0]);
    match &app.view {
        View::Main => draw_main(f, app, rows[1]),
        View::Status => draw_status(f, app, rows[1]),
        View::Text { title, lines, scroll, .. } => draw_text(f, title, lines, *scroll, rows[1]),
        View::History { job, runs, selected } => draw_history(f, job, runs, *selected, rows[1]),
        View::Logs { job, all, lines, scroll, db } => {
            let title = match (job, all) {
                (Some(j), false) => format!("Logs — {} (a: all runs in {}.log)", j, db),
                _ => format!("Logs — {}.log", db),
            };
            draw_text(f, &title, lines, *scroll, rows[1])
        }
        View::Search(x) => draw_search(f, x, rows[1]),
        View::Duplicates(x) => draw_dups(f, x, rows[1]),
        View::Browse(x) => draw_browse(f, x, rows[1]),
        View::Settings(x) => x.draw(f, &app.settings, rows[1]),
        View::Drives(x) => x.draw(f, rows[1]),
    }
    if strip > 0 {
        draw_strip(f, app, rows[2]);
    }
    draw_footer(f, app, rows[3]);
    if let Some(menu) = &app.menu {
        draw_menu(f, menu.open, menu.item, rows[1]);
    }
    if let Some(dialog) = &app.dialog {
        match (dialog, &app.view) {
            (Dialog::DupFilters { .. }, View::Duplicates(d)) => draw_dup_filters(f, d, f.area()),
            _ => draw_dialog(f, dialog, f.area()),
        }
    }
}

fn menu_x(i: usize) -> u16 {
    1 + MENUS[..i].iter().map(|(t, _)| t.chars().count() as u16 + 2).sum::<u16>()
}

fn draw_menu_bar(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let open = app.menu.as_ref().is_some_and(|m| m.open == i);
        let style = if open { Style::new().bg(Color::Cyan).fg(Color::Black) } else { BOLD };
        spans.push(Span::styled(title.to_string(), style));
        spans.push(Span::raw("  "));
    }
    let right = format!("diskUnder v{} · {} ", display_version(), app.db);
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(used + right.chars().count());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, DIM));
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::new().bg(Color::Black)), area);
}

fn draw_main(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);

    // Left: jobs, then indexed paths no job covers
    let mut items: Vec<ListItem> = Vec::new();
    let mut index_of: Vec<Option<usize>> = Vec::new(); // list row → app.selected index
    let width = cols[0].width.saturating_sub(4) as usize;
    if app.jobs.is_empty() {
        items.push(ListItem::new(Span::styled("no jobs yet — n creates one", DIM)));
        index_of.push(None);
    }
    for (i, s) in app.jobs.iter().enumerate() {
        items.push(ListItem::new(job_line(s, width)));
        index_of.push(Some(i));
    }
    if !app.paths.is_empty() {
        items.push(ListItem::new(""));
        index_of.push(None);
        items.push(ListItem::new(Span::styled("Indexed paths with no job", DIM)));
        index_of.push(None);
        for (i, p) in app.paths.iter().enumerate() {
            let label = p.root.as_ref().map_or_else(|| format!("{} (no recorded path)", p.table), |r| r.root.clone());
            let when = p.root.as_ref().map_or(String::new(), |r| format!("{} {}", r.last_mode, short_date(&r.last_started)));
            let name_w = width.saturating_sub(when.chars().count() + 2);
            items.push(ListItem::new(Line::from(vec![
                Span::raw(format!("{:<w$}", truncate_path(&label, name_w), w = name_w)),
                Span::raw("  "),
                Span::styled(when, DIM),
            ])));
            index_of.push(Some(app.jobs.len() + i));
        }
    }
    let mut state = ListState::default();
    state.select(index_of.iter().position(|i| *i == Some(app.selected)));
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Jobs "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, cols[0], &mut state);

    // Right: details of the selection
    let (title, lines) = match app.selected() {
        Selected::Job(s) => {
            let width = cols[1].width.saturating_sub(2) as usize;
            (format!(" {} ", s.job.name), job_details(s, app.recent.get(&s.job.name), width))
        }
        Selected::Path(p) => (" indexed path ".to_string(), path_lines(p).into_iter().map(Line::from).collect()),
        Selected::Nothing => (
            " welcome ".to_string(),
            vec![
                Line::from("Jobs are saved index / update runs."),
                Line::from(""),
                Line::from("  n      create a job"),
                Line::from("  t      try the test job on sample files"),
                Line::from("  m      open the menus (or F10)"),
                Line::from("  ?      all keys"),
                Line::from("  q      quit"),
            ],
        ),
    };
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)), cols[1]);
}

fn short_date(utc: &str) -> String {
    fmt_local(utc)
}

fn job_line(s: &JobSummary, width: usize) -> Line<'static> {
    let (text, style) = match &s.last {
        None => ("– never run".to_string(), DIM),
        Some(r) => {
            let (symbol, style) = mark(r, s.running);
            let st = status(r, s.running);
            let detail = if st == "completed" { short_date(&r.started) } else { format!("{} {}", st, short_date(&r.started)) };
            (format!("{} {}", symbol, detail), style)
        }
    };
    let name_w = width.saturating_sub(text.chars().count() + 2).max(8);
    Line::from(vec![
        Span::raw(format!("{:<w$}", s.job.name, w = name_w)),
        Span::raw("  "),
        Span::styled(text, style),
    ])
}

fn job_details(s: &JobSummary, recent: Option<&Vec<RunRow>>, width: usize) -> Vec<Line<'static>> {
    let j = &s.job;
    // Long paths are shortened from the left so the end — the part that differs — shows
    let fit = |label: &str, value: &str| format!("{}{}", label, truncate_path(value, width.saturating_sub(label.len())));
    let mut lines = vec![
        Line::from(format!("{} · threads {}", j.mode(), j.threads)),
        Line::from(fit("path      ", &j.path)),
        Line::from(Span::styled(fit("database  ", &j.db), DIM)),
        Line::from(fit("filters   ", &j.filters_line())),
        Line::from(""),
    ];
    match &s.last {
        None => lines.push(Line::from(Span::styled("never run — ⏎ runs it, t tests it", DIM))),
        Some(r) => {
            let st = status(r, s.running);
            let ended = match (&r.finished, r.duration) {
                (Some(_), Some(d)) => format!(", {} in {}", st, fmt_secs(d)),
                _ => format!(", {}", st),
            };
            lines.push(Line::from(vec![
                Span::styled("last run  ", BOLD),
                Span::raw(fmt_local(&r.started)),
                Span::styled(ended, status_style(&st)),
            ]));
            if r.found.is_some() {
                lines.push(Line::from(format!(
                    "  {} files · {} written · {} removed",
                    opt_count(r.found),
                    opt_count(r.written),
                    opt_count(r.removed)
                )));
                lines.push(Line::from(format!(
                    "  {} errors ({} unreadable, {} other)",
                    opt_count(r.errors()),
                    opt_count(r.errors_unreadable),
                    opt_count(r.errors_other.zip(r.errors_unwritten).map(|(a, b)| a + b))
                )));
                lines.push(Line::from(format!(
                    "duplicates  {} groups · {} wasted",
                    opt_count(r.dup_groups),
                    fmt_bytes(r.wasted.unwrap_or(0) as u64)
                )));
            }
            if let Some(previous) = recent.filter(|v| v.len() > 1) {
                let mut spans = vec![Span::styled("previous  ", BOLD)];
                for (i, p) in previous.iter().skip(1).enumerate() {
                    let (symbol, style) = mark(p, false);
                    if i > 0 {
                        spans.push(Span::raw(" · "));
                    }
                    spans.push(Span::styled(format!("{} {}", symbol, short_date(&p.started)), style));
                }
                lines.push(Line::from(spans));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("⏎ run · y dry run · e edit · l latest · h history · o errors · g logs", DIM)));
    lines
}

/// Details of an indexed path with no job
pub(crate) fn path_lines(p: &PathEntry) -> Vec<String> {
    let mut lines = Vec::new();
    match &p.root {
        Some(r) => {
            lines.push(r.root.clone());
            if let Some(d) = &p.details {
                lines.push(format!("  files       {} ({})", fmt_count(d.files), fmt_bytes(d.bytes as u64)));
                lines.push(format!("  duplicates  {} groups, {} wasted", fmt_count(d.groups), fmt_bytes(d.wasted as u64)));
            }
            let outcome = match &r.last_finished {
                Some(end) => format!("finished {}", fmt_local(end)),
                None => "did not finish".to_string(),
            };
            lines.push(format!("  last run    {} at {}, {}", r.last_mode, fmt_local(&r.last_started), outcome));
            lines.push(format!("  first run   {}", fmt_local(&r.first_run)));
            if !r.filter.types.is_empty() {
                lines.push(format!("  filters     {}", r.filter.types.describe()));
            }
            if !r.filter.paths.is_empty() {
                lines.push(format!("  paths       {}", r.filter.paths.describe()));
            }
        }
        None => {
            lines.push(format!("{} (no recorded path)", p.table));
            lines.push("  from before v4.1, or a v3 spelling — `diskunder db info` has details".into());
        }
    }
    lines.push(String::new());
    lines.push("No job covers this path. Create one (n) to run it from here and keep its history.".into());
    lines
}

/// A job run as a report: when, outcome, every count, and its errors
pub(crate) fn run_lines(r: &RunRow, errors: &[String], running: bool) -> Vec<String> {
    let mut lines = vec![
        format!("started     {}", fmt_local(&r.started)),
        format!(
            "finished    {}",
            r.finished.as_deref().map_or("—".to_string(), fmt_local)
        ),
        format!("outcome     {}", status(r, running)),
        format!("duration    {}", r.duration.map_or("—".to_string(), fmt_secs)),
        format!("run id      {}  (as in the run log)", r.run_id),
        String::new(),
        format!("found       {}", opt_count(r.found)),
        format!("skipped     {}", opt_count(r.skipped)),
        format!("written     {}", opt_count(r.written)),
        format!("removed     {}", opt_count(r.removed)),
        format!(
            "errors      {}  ({} unreadable, {} not written, {} traversal/db)",
            opt_count(r.errors()),
            opt_count(r.errors_unreadable),
            opt_count(r.errors_unwritten),
            opt_count(r.errors_other)
        ),
        format!(
            "excluded    {} files, {} folders skipped, {} rows removed as excluded",
            opt_count(r.excluded_files),
            opt_count(r.excluded_folders),
            opt_count(r.rows_excluded_removed)
        ),
        format!("duplicates  {} groups, {} wasted", opt_count(r.dup_groups), fmt_bytes(r.wasted.unwrap_or(0) as u64)),
    ];
    if !errors.is_empty() {
        lines.push(String::new());
        lines.push("Errors".into());
        lines.extend(errors.iter().map(|e| format!("  {}", e)));
    }
    lines
}

fn draw_text(f: &mut Frame, title: &str, lines: &[String], scroll: usize, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    // Keep the scroll line in view, showing as much as fits below it
    let start = scroll.min(lines.len().saturating_sub(height.max(1)));
    let shown: Vec<Line> = lines.iter().skip(start).take(height).map(|l| Line::from(l.as_str())).collect();
    let more = if lines.len() > height { format!(" {}–{} of {} ", start + 1, (start + height).min(lines.len()), lines.len()) } else { String::new() };
    f.render_widget(
        Paragraph::new(shown).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", title))
                .title_bottom(Line::from(Span::styled(more, DIM)).right_aligned()),
        ),
        area,
    );
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["NAME", "MODE", "PATH", "LAST RUN", "STATUS", "DURATION", "FILES", "ERRORS"]).style(BOLD);
    let rows: Vec<Row> = app
        .jobs
        .iter()
        .map(|s| {
            let j = &s.job;
            match &s.last {
                None => Row::new(vec![j.name.clone(), j.mode().into(), j.path.clone(), "never".into(), "—".into(), "—".into(), "—".into(), "—".into()]),
                Some(r) => {
                    let st = status(r, s.running);
                    Row::new(vec![
                        ratatui::widgets::Cell::from(j.name.clone()),
                        j.mode().into(),
                        j.path.clone().into(),
                        fmt_local(&r.started).into(),
                        ratatui::widgets::Cell::from(Span::styled(st.clone(), status_style(&st))),
                        r.duration.map_or("—".to_string(), fmt_secs).into(),
                        opt_count(r.found).into(),
                        opt_count(r.errors()).into(),
                    ])
                }
            }
        })
        .collect();
    let widths = [
        Constraint::Length(16),
        Constraint::Length(7),
        Constraint::Min(20),
        Constraint::Length(17),
        Constraint::Length(16),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(8),
    ];
    let table = Table::new(rows, widths).header(header).block(Block::default().borders(Borders::ALL).title(" Status — all jobs "));
    f.render_widget(table, area);
}

fn draw_history(f: &mut Frame, job: &str, runs: &[RunRow], selected: usize, area: Rect) {
    let header = Row::new(["STARTED", "STATUS", "DURATION", "FOUND", "WRITTEN", "REMOVED", "ERRORS", "RUN"]).style(BOLD);
    let rows: Vec<Row> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let st = status(r, false);
            let style = if i == selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            Row::new(vec![
                ratatui::widgets::Cell::from(fmt_local(&r.started)),
                ratatui::widgets::Cell::from(Span::styled(st.clone(), status_style(&st))),
                r.duration.map_or("—".to_string(), fmt_secs).into(),
                opt_count(r.found).into(),
                opt_count(r.written).into(),
                opt_count(r.removed).into(),
                opt_count(r.errors()).into(),
                r.run_id.clone().into(),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(17),
        Constraint::Length(16),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(8),
    ];
    let title = if runs.is_empty() { format!(" History — {} (never run) ", job) } else { format!(" History — {} — ⏎ errors of the selected run ", job) };
    f.render_widget(Table::new(rows, widths).header(header).block(Block::default().borders(Borders::ALL).title(title)), area);
}

fn draw_strip(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::TOP);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if let Some(run) = &app.running {
        let s = &run.started.state;
        let pct = (s.progress() * 100.0).min(100.0) as u16;
        let label = match s.total_files {
            Some(n) => format!(
                "{}  {}%  {} / {}  {}",
                run.name,
                pct,
                fmt_count((s.files_hashed + s.files_errored) as i64),
                fmt_count(n as i64),
                s.current_file
            ),
            None => format!("{}  scanning… {} files found", run.name, fmt_count(s.files_found as i64)),
        };
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::new().fg(Color::Cyan).bg(Color::Black))
                .percent(pct)
                .label(truncate_path(&label, inner.width as usize)),
            inner,
        );
    } else if let Some(done) = &app.finished {
        // Cut at the end, so the job's name and outcome always show
        let fit = |s: String| {
            let w = inner.width as usize;
            if s.chars().count() <= w { s } else { format!("{}…", s.chars().take(w.saturating_sub(1)).collect::<String>()) }
        };
        let style = if done.headline.starts_with("completed") { Style::new().fg(Color::Green) } else { status_style(&done.headline) };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Last run: ", DIM),
                    Span::styled(done.job.clone(), BOLD),
                    Span::raw(" "),
                    Span::styled(fit(done.headline.clone()), style),
                    Span::styled(" · l for details", DIM),
                ]),
                Line::from(Span::styled(fit(done.details.clone()), DIM)),
            ]),
            inner,
        );
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let keys = if app.dialog.is_some() {
        "Esc cancel"
    } else if app.menu.is_some() {
        "←→ menus · ↑↓ items · ⏎ choose · Esc close"
    } else {
        match &app.view {
            View::Main => "⏎ run · y dry run · n new · e edit · x remove · r results · t test job · m menus · c copy command · ? help · q quit",
            View::Drives(x) => x.keys(),
            View::History { .. } => "↑↓ select · ⏎ errors · c copy command · Esc back",
            View::Logs { .. } => "↑↓ PgUp PgDn scroll · a all runs · c copy command · Esc back",
            View::Search(x) if x.in_results => "↑↓ results · Tab back to the filters · c copy command · Esc back",
            View::Search(_) => "type to filter — results update as you go · Tab / ↑↓ fields · space cycles choices · ⏎ results · Esc back",
            View::Duplicates(_) => "↑↓ groups · f filters · c copy command · Esc back",
            View::Browse(_) => "↑↓ select · ⏎ / → into folder · ⌫ / ← up · c copy command · Esc back",
            View::Settings(x) if x.editing.is_some() => "type the value · ⏎ save · Esc cancel · Ctrl-U clear",
            View::Settings(_) => "↑↓ select · ⏎ edit (or toggle) · x back to default / remove group · c copy command · Esc back",
            _ => "↑↓ PgUp PgDn scroll · c copy command · Esc back",
        }
    };
    let top = match &app.message {
        Some(m) => Line::from(Span::styled(m.clone(), Style::new().fg(Color::Yellow))),
        None => Line::from(vec![Span::styled("$ ", DIM), Span::raw(app.command())]),
    };
    f.render_widget(Paragraph::new(vec![top, Line::from(Span::styled(keys, DIM))]), area);
}

fn draw_menu(f: &mut Frame, open: usize, item: usize, body: Rect) {
    let (_, items) = MENUS[open];
    let width = items.iter().map(|(t, k, _)| t.chars().count() + k.chars().count() + 4).max().unwrap_or(10) as u16 + 2;
    let area = Rect { x: body.x + menu_x(open), y: body.y, width: width.min(body.width), height: (items.len() as u16 + 2).min(body.height) };
    let rows: Vec<ListItem> = items
        .iter()
        .map(|(t, k, _)| {
            let pad = (width as usize).saturating_sub(t.chars().count() + k.chars().count() + 4);
            ListItem::new(Line::from(vec![Span::raw(format!(" {}{}", t, " ".repeat(pad))), Span::styled(format!(" {} ", k), DIM)]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(item));
    f.render_widget(Clear, area);
    f.render_stateful_widget(
        List::new(rows)
            .block(Block::default().borders(Borders::ALL))
            .highlight_style(Style::new().bg(Color::Cyan).fg(Color::Black)),
        area,
        &mut state,
    );
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_dialog(f: &mut Frame, dialog: &Dialog, area: Rect) {
    match dialog {
        Dialog::Help => {
            let lines: Vec<Line> = [
                "Main window",
                "  ↑↓ / j k   select a job or path",
                "  ⏎          run the job (or Latest for a path)",
                "  y          dry run: what the job would change",
                "  n / e / x  new, edit, remove a job",
                "  t          test job: try diskunder on sample files",
                "  s          status of every job",
                "  l h o g    latest · history · errors · logs",
                "  r          Results menu    m / F10  menus",
                "  c          copy the command shown in the footer",
                "  q          quit (asks if a run is in progress)",
                "",
                "Views and menus",
                "  Esc        back / close",
                "  m          back to the menu you came from",
                "  ↑↓ PgUp PgDn Home End   scroll",
                "",
                "Every screen shows its command line in the footer.",
            ]
            .iter()
            .map(|l| Line::from(*l))
            .collect();
            let area = centered(64, lines.len() as u16 + 2, area);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Keys — any key closes ")), area);
        }
        Dialog::Reports { selected } => {
            let area = centered(44, REPORTS.len() as u16 + 2, area);
            let items: Vec<ListItem> = REPORTS.iter().map(|(label, _)| ListItem::new(format!(" {}", label))).collect();
            let mut state = ListState::default();
            state.select(Some(*selected));
            f.render_widget(Clear, area);
            f.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Reports — ⏎ choose "))
                    .highlight_style(Style::new().bg(Color::Cyan).fg(Color::Black)),
                area,
                &mut state,
            );
        }
        Dialog::DupFilters { .. } => {} // drawn with the Duplicates view, which owns the fields
        Dialog::Vacuum => {
            let area = centered(64, 5, area);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from("Compact the database file? It may take a while on a big index."),
                    Line::from(Span::styled("y vacuum · Esc cancel", DIM)),
                ])
                .block(Block::default().borders(Borders::ALL).title(" Vacuum ")),
                area,
            );
        }
        Dialog::DropIndex { items, selected, confirm } => {
            let height = items.len().min(14) as u16 + 4;
            let area = centered(90, height, area);
            f.render_widget(Clear, area);
            if *confirm {
                let it = &items[*selected];
                let mut lines = vec![Line::from(format!("Remove the index of {}? This can't be undone.", it.label))];
                if let Some(job) = &it.job {
                    lines.push(Line::from(Span::styled(
                        format!("Job {} uses it — its next run will index everything again.", job),
                        Style::new().fg(Color::Yellow),
                    )));
                }
                lines.push(Line::from(Span::styled("y remove · Esc cancel", DIM)));
                f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Drop path ")), area);
            } else {
                let rows: Vec<ListItem> = items
                    .iter()
                    .map(|it| {
                        let job = it.job.as_ref().map_or(String::new(), |j| format!("   (job {})", j));
                        // Long paths shortened from the left so the job note stays visible
                        let room = (area.width as usize).saturating_sub(job.chars().count() + 5);
                        ListItem::new(Line::from(vec![Span::raw(format!(" {}", truncate_path(&it.label, room))), Span::styled(job, DIM)]))
                    })
                    .collect();
                let mut state = ListState::default();
                state.select(Some(*selected));
                f.render_stateful_widget(
                    List::new(rows)
                        .block(Block::default().borders(Borders::ALL).title(" Drop path — ⏎ choose · Esc cancel "))
                        .highlight_style(Style::new().bg(Color::Cyan).fg(Color::Black)),
                    area,
                    &mut state,
                );
            }
        }
        Dialog::Input { purpose, value } => {
            let (title, help) = match purpose {
                InputFor::Open => (" Open database ", "an index database file — created by the first run if it doesn't exist"),
                InputFor::Backup => (" Back up database ", "where to write the copy — a new file; safe while the database is in use"),
            };
            let area = centered(100, 5, area);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(format!("{}▏", value), Style::new().fg(Color::Cyan))),
                    Line::from(Span::styled(format!("{} · ⏎ ok · Esc cancel · Ctrl-U clear", help), DIM)),
                ])
                .block(Block::default().borders(Borders::ALL).title(title)),
                area,
            );
        }
        Dialog::QuitWhileRunning => {
            let area = centered(64, 5, area);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from("A run is in progress. Quit anyway?"),
                    Line::from(Span::styled("It will be recorded as stopped by user.   y quit · Esc keep running", DIM)),
                ])
                .block(Block::default().borders(Borders::ALL).title(" Quit ")),
                area,
            );
        }
        Dialog::RemoveJob { name, drop_index } => {
            let area = centered(64, 6, area);
            f.render_widget(Clear, area);
            let check = if *drop_index { "[x]" } else { "[ ]" };
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Remove job {} and its run history?", name)),
                    Line::from(format!("{} also remove its index (space toggles)", check)),
                    Line::from(Span::styled("y remove · Esc cancel", DIM)),
                ])
                .block(Block::default().borders(Borders::ALL).title(" Remove job ")),
                area,
            );
        }
        Dialog::TestJob { selected, made, ran, seen, clash } => {
            let done = [*made, *ran, *seen, false];
            let mut lines: Vec<Line> = vec![
                Line::from("A job over generated sample files, to see diskunder work without real data."),
                Line::from(""),
            ];
            for (i, ((label, about), done)) in TEST_STEPS.iter().zip(done).enumerate() {
                let style = if i == *selected { Style::new().bg(Color::Cyan).fg(Color::Black) } else { Style::new() };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} {}  {:<12}", i + 1, if done { "✓" } else { " " }, label), style),
                    Span::styled(format!(" {}", about), DIM),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(if *clash {
                Line::from(Span::styled("You already have a job called test — rename or remove it first.", Style::new().fg(Color::Yellow)))
            } else {
                Line::from(Span::styled("↑↓ step · ⏎ do it (or 1–4) · c copy command · Esc close · m menus", DIM))
            });
            let area = centered(96, lines.len() as u16 + 2, area);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Test job ")), area);
        }
        Dialog::Form(form) => {
            let height = form.fields.len() as u16 * 2 + 5;
            let area = centered(80, height, area);
            f.render_widget(Clear, area);
            let mut lines = Vec::new();
            for (i, field) in form.fields.iter().enumerate() {
                let focused = i == form.focus;
                let label = Span::styled(format!("{:>14}  ", field.label), if focused { BOLD } else { Style::new() });
                let value = if field.locked {
                    Span::styled(field.value.clone(), DIM)
                } else if focused {
                    Span::styled(format!("{}▏", field.value), Style::new().fg(Color::Cyan))
                } else {
                    Span::raw(field.value.clone())
                };
                lines.push(Line::from(vec![label, value]));
                lines.push(Line::from(Span::styled(format!("{:>16}{}", "", if focused { field.hint } else { "" }), DIM)));
            }
            lines.push(match &form.error {
                Some(e) => Line::from(Span::styled(e.clone(), Style::new().fg(Color::Red))),
                None => Line::from(Span::styled("Tab / ↑↓ move · ⏎ save · Esc cancel · Ctrl-U clear field", DIM)),
            });
            f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(form.title())), area);
        }
    }
}

/// Labelled fields, the focused one highlighted with its hint
fn field_lines(fields: &Fields, focused: bool) -> Vec<Line<'static>> {
    fields
        .items
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let on = focused && i == fields.focus;
            let value = if f.choices.is_empty() {
                if on { format!("{}▏", f.value) } else { f.value.clone() }
            } else {
                format!("‹ {} ›", f.value)
            };
            let mut spans = vec![
                Span::styled(format!("{:>13}  ", f.label), if on { BOLD } else { Style::new() }),
                Span::styled(value, if on { Style::new().fg(Color::Cyan) } else { Style::new() }),
            ];
            if on {
                spans.push(Span::styled(format!("   {}", f.hint), DIM));
            }
            Line::from(spans)
        })
        .collect()
}

fn draw_search(f: &mut Frame, s: &Search, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(s.fields.items.len() as u16 + 2), Constraint::Min(3)])
        .split(area);
    f.render_widget(
        Paragraph::new(field_lines(&s.fields, !s.in_results))
            .block(Block::default().borders(Borders::ALL).title(format!(" Search — {} ", s.target.label))),
        parts[0],
    );
    let title = match &s.error {
        Some(e) => format!(" {} ", e),
        None if s.total > s.results.len() => format!(" {} matches — showing the first {} ", fmt_count(s.total as i64), s.results.len()),
        None => format!(" {} matches ", fmt_count(s.total as i64)),
    };
    let header = Row::new(["SIZE", "MODIFIED", "PATH"]).style(BOLD);
    let rows: Vec<Row> = s
        .results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let style = if s.in_results && i == s.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            let modified = if r.modified.is_empty() { "-".to_string() } else { fmt_local(&r.modified) };
            Row::new(vec![fmt_bytes(r.size as u64), modified, truncate_path(&r.path, parts[1].width.saturating_sub(32) as usize)]).style(style)
        })
        .collect();
    let skip = if s.in_results { s.selected.saturating_sub(parts[1].height.saturating_sub(4) as usize) } else { 0 };
    let rows: Vec<Row> = rows.into_iter().skip(skip).collect();
    let title_style = if s.error.is_some() { Style::new().fg(Color::Red) } else { Style::new() };
    f.render_widget(
        Table::new(rows, [Constraint::Length(10), Constraint::Length(17), Constraint::Min(10)])
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(Span::styled(title, title_style))),
        parts[1],
    );
}

fn draw_dups(f: &mut Frame, d: &Dups, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let active: Vec<String> = d
        .filters
        .items
        .iter()
        .filter(|f| !f.value.trim().is_empty())
        .map(|f| format!("{} {}", f.label.to_lowercase(), f.value.trim()))
        .collect();
    let filters = if active.is_empty() { "no filters".to_string() } else { active.join(" · ") };
    let title = match &d.error {
        Some(e) => format!(" Duplicates — {} — {} ", d.target.label, e),
        None => format!(
            " Duplicates — {} — {} groups · {} wasted — {} (f to change) ",
            d.target.label,
            fmt_count(d.groups.len() as i64),
            fmt_bytes(d.wasted() as u64),
            filters
        ),
    };
    let header = Row::new(["WASTED", "SIZE × COPIES", "NAME"]).style(BOLD);
    let visible = parts[0].height.saturating_sub(4) as usize;
    let skip = d.selected.saturating_sub(visible);
    let rows: Vec<Row> = d
        .groups
        .iter()
        .enumerate()
        .skip(skip)
        .map(|(i, g)| {
            let style = if i == d.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            Row::new(vec![fmt_bytes(g.wasted as u64), format!("{} × {}", fmt_bytes(g.size as u64), g.copies), g.name.clone()]).style(style)
        })
        .collect();
    f.render_widget(
        Table::new(rows, [Constraint::Length(11), Constraint::Length(18), Constraint::Min(10)])
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(title)),
        parts[0],
    );
    let width = parts[1].width.saturating_sub(34) as usize;
    let lines: Vec<Line> = d
        .members
        .iter()
        .map(|m| {
            let when = if m.modified.is_empty() { "-".to_string() } else { fmt_local(&m.modified) };
            let mut spans = vec![Span::styled(format!("{:<17} ", when), DIM), Span::raw(truncate_path(&m.path, width))];
            if m.hard_link {
                spans.push(Span::styled("  (hard link)", DIM));
            }
            Line::from(spans)
        })
        .collect();
    let name = d.groups.get(d.selected).map_or(String::new(), |g| format!(" {} — copies ", g.name));
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(name)), parts[1]);
}

/// The duplicate filters, over the Duplicates view
pub(crate) fn draw_dup_filters(f: &mut Frame, d: &Dups, area: Rect) {
    let mut lines = field_lines(&d.filters, true);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Tab / ↑↓ move · ⏎ apply · Esc cancel · Ctrl-U clear field", DIM)));
    let area = centered(90, lines.len() as u16 + 2, area);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Duplicate filters ")), area);
}

fn draw_browse(f: &mut Frame, b: &Browse, area: Rect) {
    let title = match &b.error {
        Some(e) => format!(" Browse — {} ", e),
        None => format!(" {} — {} files · {} ", b.folder, fmt_count(b.total.0), fmt_bytes(b.total.1 as u64)),
    };
    let header = Row::new(["FILES", "SIZE", "SHARE", "DUPLICATED", "FOLDER"]).style(BOLD);
    let visible = area.height.saturating_sub(4) as usize;
    let skip = b.selected.saturating_sub(visible);
    let rows: Vec<Row> = b
        .rows
        .iter()
        .enumerate()
        .skip(skip)
        .map(|(i, r)| {
            let style = if i == b.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            let share = if b.total.1 > 0 { r.bytes as f64 * 100.0 / b.total.1 as f64 } else { 0.0 };
            let name = if r.folder.is_some() { format!("{}/", r.name) } else { r.name.clone() };
            let dup = if r.duplicated > 0 { fmt_bytes(r.duplicated as u64) } else { "—".to_string() };
            Row::new(vec![fmt_count(r.files), fmt_bytes(r.bytes as u64), format!("{:.1}%", share), dup, name]).style(style)
        })
        .collect();
    f.render_widget(
        Table::new(
            rows,
            [Constraint::Length(10), Constraint::Length(10), Constraint::Length(7), Constraint::Length(11), Constraint::Min(10)],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}
