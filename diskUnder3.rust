// diskUnder v3.0 — TUI progress and live duplicate results
// See changes.md for full change history
//
// Additional entries required in Cargo.toml [dependencies]:
//   ratatui   = "0.28"
//   crossterm = "0.28"

use blake3;
use chrono::{DateTime, Utc};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use md5;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame, Terminal,
};
use rayon::prelude::*;
use rusqlite::{params, Connection, Result as DbResult};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, IsTerminal, Read, Stdout},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, SystemTime},
};
use structopt::StructOpt;
use walkdir::WalkDir;

const BATCH_SIZE: usize = 1_000;
const READ_CHUNK: usize = 65_536; // 64 KB
const TICK: Duration = Duration::from_millis(100); // UI refresh rate

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(StructOpt)]
#[structopt(name = "diskunder", about = "File indexer with live TUI progress")]
struct Cli {
    #[structopt(subcommand)]
    cmd: Command,

    /// SQLite database file (created if absent)
    #[structopt(long, default_value = "fileindex.db", global = true)]
    db: String,

    /// Parallel hashing threads — keep low for network shares (e.g. --threads 4)
    #[structopt(long, default_value = "4", global = true)]
    threads: usize,

    /// Disable the TUI and print plain progress lines instead.
    /// Enabled automatically when stdout is not a terminal (cron, SSH, pipes).
    #[structopt(long, global = true)]
    no_tui: bool,
}

#[derive(StructOpt)]
enum Command {
    /// Full index of PATH
    Index { path: String },
    /// Re-index files modified in the last 24 hours under PATH
    Update { path: String },
}

// ─── Worker → UI messages ────────────────────────────────────────────────────

enum WorkerMsg {
    /// One file discovered during directory traversal
    FileFound,
    /// One file successfully hashed
    FileHashed {
        path: String,
        size: u64,
        checksum: String,
    },
    /// A file or traversal entry that could not be read
    FileError { path: String, error: String },
    /// A batch of N rows committed to SQLite
    BatchWritten(usize),
    /// Directory traversal finished — total file count now known
    TraversalComplete(usize),
    /// All work done
    Done,
}

// ─── Duplicate tracking ──────────────────────────────────────────────────────

struct DupGroup {
    size: u64,
    paths: Vec<String>,
    wasted: u64, // size × (copies − 1)
}

// ─── Application state ───────────────────────────────────────────────────────

struct AppState {
    mode: String,
    target_path: String,
    db_path: String,

    files_found: usize,
    files_hashed: usize,
    files_written: usize,
    files_errored: usize,
    total_files: Option<usize>, // set once traversal is complete

    current_file: String,

    // In-memory checksum index for live duplicate detection.
    // For very large runs (millions of files) this will grow proportionally.
    checksum_map: HashMap<String, (u64, Vec<String>)>, // checksum → (size, paths)
    top_dups: Vec<DupGroup>,                           // top 10 by wasted bytes

    log: Vec<String>,
    done: bool,
}

impl AppState {
    fn new(mode: &str, target_path: &str, db_path: &str) -> Self {
        AppState {
            mode: mode.to_owned(),
            target_path: target_path.to_owned(),
            db_path: db_path.to_owned(),
            files_found: 0,
            files_hashed: 0,
            files_written: 0,
            files_errored: 0,
            total_files: None,
            current_file: String::new(),
            checksum_map: HashMap::new(),
            top_dups: Vec::new(),
            log: Vec::new(),
            done: false,
        }
    }

    fn progress(&self) -> f64 {
        match self.total_files {
            // Count errors as processed — otherwise permission-denied files on /Users/
            // or a network share leave the bar stuck below 100% when the run is done.
            Some(n) if n > 0 => (self.files_hashed + self.files_errored) as f64 / n as f64,
            _ => 0.0,
        }
    }

    fn push_log(&mut self, msg: String) {
        self.log.push(msg);
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    fn record_hashed(&mut self, path: String, size: u64, checksum: String) {
        self.files_hashed += 1;
        self.current_file = path.clone();

        let entry = self
            .checksum_map
            .entry(checksum)
            .or_insert_with(|| (size, Vec::new()));
        entry.1.push(path);

        // Rebuild top-10 list whenever a new duplicate group is created or grows
        if entry.1.len() >= 2 {
            self.rebuild_top_dups();
        }
    }

    fn rebuild_top_dups(&mut self) {
        let mut groups: Vec<DupGroup> = self
            .checksum_map
            .values()
            .filter(|(_, paths)| paths.len() >= 2)
            .map(|(size, paths)| {
                let wasted = size * (paths.len() as u64 - 1);
                DupGroup {
                    size: *size,
                    paths: paths.clone(),
                    wasted,
                }
            })
            .collect();

        groups.sort_unstable_by(|a, b| b.wasted.cmp(&a.wasted));
        groups.truncate(10);
        self.top_dups = groups;
    }

    fn apply(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::FileFound => self.files_found += 1,
            WorkerMsg::FileHashed { path, size, checksum } => {
                self.record_hashed(path, size, checksum);
            }
            WorkerMsg::FileError { path, error } => {
                self.files_errored += 1;
                self.push_log(format!("warn: {} — {}", path, error));
            }
            WorkerMsg::BatchWritten(n) => self.files_written += n,
            WorkerMsg::TraversalComplete(n) => {
                self.total_files = Some(n);
                self.push_log(format!("traversal complete — {} files found", n));
            }
            WorkerMsg::Done => {
                self.done = true;
                self.push_log(format!(
                    "done — {} written, {} errors, {} duplicate groups",
                    self.files_written,
                    self.files_errored,
                    self.top_dups.len()
                ));
            }
        }
    }
}

// ─── File entry ──────────────────────────────────────────────────────────────

struct FileEntry {
    path: String,
    filename: String,
    size: u64,
    modified_date: String,
    access_date: String,
    change_date: String,
    checksum: String,
    extension: String,
    directory: String,
}

impl FileEntry {
    fn from_path(path: &Path) -> Result<Self, std::io::Error> {
        let metadata = fs::metadata(path)?;
        let checksum = hash_streaming(path)?;

        Ok(FileEntry {
            path: path.to_string_lossy().into_owned(),
            filename: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size: metadata.len(),
            modified_date: metadata
                .modified()
                .ok()
                .map(fmt_time)
                .unwrap_or_default(),
            access_date: metadata
                .accessed()
                .ok()
                .map(fmt_time)
                .unwrap_or_default(),
            change_date: metadata
                .created()
                .ok()
                .map(fmt_time)
                .unwrap_or_default(),
            checksum,
            extension: path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default(),
            directory: path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })
    }
}

fn hash_streaming(path: &Path) -> Result<String, std::io::Error> {
    let mut reader = BufReader::with_capacity(READ_CHUNK, File::open(path)?);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; READ_CHUNK];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn fmt_time(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ─── Database ────────────────────────────────────────────────────────────────

fn create_table(conn: &Connection, path: &str) -> DbResult<String> {
    let table_name = format!("files_{:x}", md5::compute(path));
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {t} (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                path          TEXT NOT NULL UNIQUE,
                filename      TEXT NOT NULL,
                size          INTEGER NOT NULL,
                modified_date TEXT,
                access_date   TEXT,
                change_date   TEXT,
                checksum      TEXT,
                extension     TEXT,
                directory     TEXT,
                created_at    TEXT DEFAULT CURRENT_TIMESTAMP
            )",
            t = table_name
        ),
        [],
    )?;
    conn.execute(
        &format!(
            "CREATE INDEX IF NOT EXISTS idx_{t}_path ON {t}(path)",
            t = table_name
        ),
        [],
    )?;
    Ok(table_name)
}

fn create_indexes(conn: &Connection, t: &str) -> DbResult<()> {
    for col in &["size", "filename", "checksum", "extension", "directory", "access_date", "modified_date"] {
        conn.execute(
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_{c} ON {t}({c})", t = t, c = col),
            [],
        )?;
    }
    Ok(())
}

fn commit_batch(conn: &mut Connection, t: &str, batch: &[FileEntry], upsert: bool) -> DbResult<usize> {
    let sql = if upsert {
        format!(
            "INSERT INTO {t} (path,filename,size,modified_date,access_date,change_date,checksum,extension,directory)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(path) DO UPDATE SET
               size=excluded.size, modified_date=excluded.modified_date,
               access_date=excluded.access_date, change_date=excluded.change_date,
               checksum=excluded.checksum, extension=excluded.extension,
               directory=excluded.directory",
            t = t
        )
    } else {
        format!(
            "INSERT OR IGNORE INTO {t}
             (path,filename,size,modified_date,access_date,change_date,checksum,extension,directory)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            t = t
        )
    };

    let tx = conn.transaction()?;
    let mut count = 0usize;
    for e in batch {
        count += tx.execute(
            &sql,
            params![e.path, e.filename, e.size, e.modified_date, e.access_date,
                    e.change_date, e.checksum, e.extension, e.directory],
        )?;
    }
    tx.commit()?;
    Ok(count)
}

// ─── Worker thread ───────────────────────────────────────────────────────────

fn run_worker(path: String, db_path: String, threads: usize, upsert: bool, tx: Sender<WorkerMsg>) {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let mut conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(WorkerMsg::FileError { path: db_path, error: e.to_string() });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    };
    let _ = conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY; PRAGMA cache_size=100000;",
    );

    let table_name = match create_table(&conn, &path) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(WorkerMsg::FileError { path, error: e.to_string() });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    };

    // ── Traversal ──
    let cutoff = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
    let mut files: Vec<PathBuf> = Vec::new();

    for entry in WalkDir::new(&path) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                if upsert {
                    // update mode: only files modified in last 24h
                    match e.metadata().ok().and_then(|m| m.modified().ok()) {
                        Some(t) if t > cutoff => {
                            let _ = tx.send(WorkerMsg::FileFound);
                            files.push(e.path().to_owned());
                        }
                        _ => {}
                    }
                } else {
                    let _ = tx.send(WorkerMsg::FileFound);
                    files.push(e.path().to_owned());
                }
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(WorkerMsg::FileError {
                    path: e.path().map(|p| p.display().to_string()).unwrap_or_default(),
                    error: e.to_string(),
                });
            }
        }
    }

    let total = files.len();
    let _ = tx.send(WorkerMsg::TraversalComplete(total));

    // ── Hashing + DB writes in batches ──
    for chunk in files.chunks(BATCH_SIZE) {
        let results: Vec<Result<FileEntry, (String, String)>> = chunk
            .par_iter()
            .map(|p| FileEntry::from_path(p).map_err(|e| (p.display().to_string(), e.to_string())))
            .collect();

        let mut batch: Vec<FileEntry> = Vec::with_capacity(results.len());
        for result in results {
            match result {
                Ok(entry) => {
                    let _ = tx.send(WorkerMsg::FileHashed {
                        path: entry.path.clone(),
                        size: entry.size,
                        checksum: entry.checksum.clone(),
                    });
                    batch.push(entry);
                }
                Err((path, error)) => {
                    let _ = tx.send(WorkerMsg::FileError { path, error });
                }
            }
        }

        if !batch.is_empty() {
            match commit_batch(&mut conn, &table_name, &batch, upsert) {
                Ok(n) => {
                    let _ = tx.send(WorkerMsg::BatchWritten(n));
                }
                Err(e) => {
                    let _ = tx.send(WorkerMsg::FileError {
                        path: "db".to_string(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }

    if !upsert {
        let _ = create_indexes(&conn, &table_name);
    }

    let _ = tx.send(WorkerMsg::Done);
}

// ─── UI helpers ──────────────────────────────────────────────────────────────

fn fmt_bytes(b: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    if b >= GB {
        format!("{:.1} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{} B", b)
    }
}

/// Truncate a path string to fit within `width`, adding a leading ellipsis.
fn truncate_path(s: &str, width: usize) -> String {
    if s.len() <= width {
        s.to_owned()
    } else {
        format!("…{}", &s[s.len().saturating_sub(width.saturating_sub(1))..])
    }
}

// ─── UI rendering ────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, state: &AppState) {
    let area = f.area();

    // Root: header / body / log
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(10),    // body
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
        "  diskUnder v3.0   mode: {}   path: {}   db: {}   (q to quit)",
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

    let status_label = if state.done { "  complete" } else { "  running…" };
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
            Span::styled(
                format!(" / {}", total_str),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Hashed  "),
            Span::styled(
                format!("{:>9}", state.files_hashed),
                Style::default().fg(Color::White),
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
                format!("{:>9}", state.files_errored),
                Style::default().fg(if state.files_errored > 0 {
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
                        "   {} × {} copies",
                        fmt_bytes(group.size),
                        group.paths.len()
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
        let total_wasted: u64 = state.top_dups.iter().map(|g| g.wasted).sum();
        items.push(ListItem::new(Span::styled(
            format!(
                "  top {} groups — {} total wasted (all groups)",
                state.top_dups.len(),
                fmt_bytes(total_wasted)
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    let dup_count = state.top_dups.len();
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

// ─── Plain-text fallback (no TTY / --no-tui) ─────────────────────────────────

fn run_plain(state: &mut AppState, rx: Receiver<WorkerMsg>) {
    eprintln!("{} {}", state.mode, state.target_path);

    for msg in rx {
        let print_progress = matches!(msg, WorkerMsg::BatchWritten(_));
        let done = matches!(msg, WorkerMsg::Done);

        state.apply(msg);

        if print_progress {
            let processed = state.files_hashed + state.files_errored;
            let total_str = state
                .total_files
                .map(|n| n.to_string())
                .unwrap_or_else(|| "…".into());
            eprintln!(
                "  {}/{} processed — {} written, {} errors/skipped",
                processed, total_str, state.files_written, state.files_errored
            );
        }

        if done {
            break;
        }
    }

    eprintln!(
        "complete: {} files — {} written, {} errors/skipped, {} duplicate groups",
        state.total_files.unwrap_or(0),
        state.files_written,
        state.files_errored,
        state.top_dups.len(),
    );
    if !state.top_dups.is_empty() {
        let wasted: u64 = state.top_dups.iter().map(|g| g.wasted).sum();
        eprintln!("wasted (top {}): {}", state.top_dups.len(), fmt_bytes(wasted));
    }
}

// ─── UI event loop ───────────────────────────────────────────────────────────

fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    rx: Receiver<WorkerMsg>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| draw(f, state))?;

        // Drain all queued worker messages before the next redraw
        loop {
            match rx.try_recv() {
                Ok(msg) => state.apply(msg),
                Err(_) => break,
            }
        }

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

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::from_args();

    let (mode, path) = match &args.cmd {
        Command::Index { path } => ("index", path.clone()),
        Command::Update { path } => ("update", path.clone()),
    };

    if !Path::new(&path).exists() {
        eprintln!("error: path not found: {}", path);
        std::process::exit(1);
    }

    let upsert = matches!(args.cmd, Command::Update { .. });
    let db_path = args.db.clone();
    let threads = args.threads;

    // Use plain-text mode if --no-tui is set OR stdout is not an interactive terminal
    // (e.g. cron job, SSH without TTY, piped output)
    let plain = args.no_tui || !std::io::stdout().is_terminal();

    // Start worker on a background thread
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    let path_w = path.clone();
    let db_w = db_path.clone();
    std::thread::spawn(move || run_worker(path_w, db_w, threads, upsert, tx));

    let mut state = AppState::new(mode, &path, &db_path);

    if plain {
        run_plain(&mut state, rx);
    } else {
        // Initialise TUI terminal
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

        let result = run_ui(&mut terminal, &mut state, rx);

        // Always restore terminal, even on error
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        if let Err(e) = result {
            eprintln!("UI error: {}", e);
            std::process::exit(1);
        }

        // Print plain-text summary after TUI exits
        println!("\ndiskUnder — complete");
        println!("  files found   : {}", state.files_found);
        println!("  files written : {}", state.files_written);
        println!("  errors        : {}", state.files_errored);
        println!("  dup groups    : {}", state.top_dups.len());
        if !state.top_dups.is_empty() {
            let wasted: u64 = state.top_dups.iter().map(|g| g.wasted).sum();
            println!("  wasted (top{:2}): {}", state.top_dups.len(), fmt_bytes(wasted));
        }
    }

    Ok(())
}
