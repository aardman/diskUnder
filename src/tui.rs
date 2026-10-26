//! Full-screen progress display

use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame, Terminal,
};
use std::{
    io::Stdout,
    sync::mpsc::Receiver,
    time::Duration,
};

use crate::cli::display_version;
use crate::fmt::{fmt_bytes, truncate_path};
use crate::progress::AppState;
use crate::runlog::RunLog;
use crate::scan::WorkerMsg;

const TICK: Duration = Duration::from_millis(100); // UI refresh rate

// ─── UI rendering ────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, state: &AppState) {
    let area = f.area();

    // Root: header / body / log
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(11),    // body
            Constraint::Length(7),  // activity log
        ])
        .split(area);

    draw_header(f, state, root[0]);

    // Body: progress panel (left) + duplicates panel (right)
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(root[1]);

    draw_progress(f, state, body[0]);
    draw_duplicates(f, state, body[1]);
    draw_log(f, state, root[2]);
}

fn draw_header(f: &mut Frame, state: &AppState, area: Rect) {
    let text = format!(
        "  diskUnder v{}   mode: {}   path: {}   db: {}   (q to quit)",
        display_version(),
        state.mode, state.target_path, state.db_path
    );
    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL))
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(p, area);
}

fn draw_progress(f: &mut Frame, state: &AppState, area: Rect) {
    let pct = (state.progress() * 100.0).min(100.0) as u16;
    let total_str = state
        .total_files
        .map(|n| format!("{}", n))
        .unwrap_or_else(|| "scanning…".into());

    let status_label = if state.done { "  complete".to_string() } else { format!("  {}…", state.stage_label()) };
    let status_color = if state.done { Color::Green } else { Color::Yellow };

    let path_display = truncate_path(&state.current_file, area.width.saturating_sub(4) as usize);

    let lines = vec![
        Line::from(Span::styled(
            status_label,
            Style::default().fg(status_color).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Found   "),
            Span::styled(
                format!("{:>9}", state.files_found),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Skipped "),
            Span::styled(
                format!("{:>9}", state.files_skipped),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("  {}", state.skipped_label()),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Recorded"),
            Span::styled(
                format!("{:>9}", state.files_recorded),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!(" / {}", total_str),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Read    "),
            Span::styled(
                format!("{:>9}", state.full_hashed),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("  in full · {} quick checks", state.quick_checked),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Written "),
            Span::styled(
                format!("{:>9}", state.files_written),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Errors  "),
            Span::styled(
                format!("{:>9}", state.errors_total()),
                Style::default().fg(if state.errors_total() > 0 {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", path_display),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default().title(" Progress ").borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);

    // Progress bar pinned to the bottom of the panel
    if area.height >= 4 {
        let bar = Rect {
            x: area.x + 1,
            y: area.y + area.height - 2,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
                .percent(pct)
                .label(format!("{}%", pct)),
            bar,
        );
    }
}

fn draw_duplicates(f: &mut Frame, state: &AppState, area: Rect) {
    let max_path_width = area.width.saturating_sub(6) as usize;
    let mut items: Vec<ListItem> = Vec::new();

    if state.top_dups.is_empty() {
        items.push(ListItem::new(Span::styled(
            "  No duplicates found yet…",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for group in &state.top_dups {
            // Group heading: wasted space + per-file size + copy count
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {} wasted", fmt_bytes(group.wasted)),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "   {} × {} copies{}",
                        fmt_bytes(group.size),
                        group.copies,
                        match group.paths.len() - group.copies {
                            0 => String::new(),
                            1 => " (+1 hard link)".to_string(),
                            n => format!(" (+{} hard links)", n),
                        }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ])));

            // Up to 3 paths per group
            for path in group.paths.iter().take(3) {
                items.push(ListItem::new(Span::styled(
                    format!("    {}", truncate_path(path, max_path_width)),
                    Style::default().fg(Color::White),
                )));
            }
            if group.paths.len() > 3 {
                items.push(ListItem::new(Span::styled(
                    format!("    … and {} more", group.paths.len() - 3),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            items.push(ListItem::new(""));
        }

        // Summary footer
        items.push(ListItem::new(Span::styled(
            format!(
                "  top {} of {} groups — {} wasted across all groups",
                state.top_dups.len(),
                state.dup_keys.len(),
                fmt_bytes(state.dup_wasted)
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    let dup_count = state.dup_keys.len();
    let title = if dup_count == 0 {
        " Duplicates ".to_string()
    } else {
        format!(" Duplicates ({} groups found) ", dup_count)
    };

    f.render_widget(
        List::new(items).block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn draw_log(f: &mut Frame, state: &AppState, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let start = state.log.len().saturating_sub(visible);

    let items: Vec<ListItem> = state.log[start..]
        .iter()
        .map(|msg| {
            let color = if msg.starts_with("warn") || msg.starts_with("error") {
                Color::Red
            } else if msg.contains("complete") || msg.contains("done") {
                Color::Green
            } else {
                Color::DarkGray
            };
            ListItem::new(Span::styled(
                format!("  {}", msg),
                Style::default().fg(color),
            ))
        })
        .collect();

    f.render_widget(
        List::new(items).block(Block::default().title(" Activity ").borders(Borders::ALL)),
        area,
    );
}

// ─── UI event loop ───────────────────────────────────────────────────────────

pub(crate) fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    rx: Receiver<WorkerMsg>,
    runlog: &mut RunLog,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| draw(f, state))?;

        // Drain all queued worker messages before the next redraw
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    runlog.observe(&msg);
                    state.apply(msg);
                }
                Err(_) => break,
            }
        }
        state.refresh_top_dups();

        // Non-blocking keyboard poll — timeout drives the redraw rate
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
