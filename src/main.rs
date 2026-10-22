// diskUnder — file indexer with live TUI progress, duplicate detection and database maintenance
// See changes.md for full change history

mod cli;
mod db;
mod dryrun;
mod filters;
mod fmt;
mod maintenance;
mod output;
mod plain;
mod progress;
mod query;
mod report;
mod runlog;
mod scan;
mod search;
mod tui;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    fs,
    io::IsTerminal,
    sync::mpsc,
};
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::dryrun::run_dry_run;
use crate::fmt::fmt_bytes;
use crate::maintenance::run_db_command;
use crate::plain::run_plain;
use crate::progress::AppState;
use crate::report::{run_report, ReportOptions};
use crate::runlog::RunLog;
use crate::scan::{WorkerMsg, run_worker};
use crate::search::{run_export, run_search, SearchOptions};
use crate::tui::run_ui;

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();

    let mut runlog = RunLog::new(&args.db, args.log.as_deref(), args.no_log);

    let (mode, typed, dry_run, filter) = match &args.cmd {
        Command::Index { path, dry_run, types } => ("index", path.clone(), *dry_run, types.filter()),
        Command::Update { path, dry_run, types } => ("update", path.clone(), *dry_run, types.filter()),
        Command::Db { action } => {
            let code = run_db_command(&args.db, action, &mut runlog);
            report_log_failure(&runlog);
            std::process::exit(code);
        }
        // Read-only: never logged
        Command::Search { path, filters, sort, limit, format, paths, null, long } => {
            let options = SearchOptions {
                path: path.as_deref(),
                filters,
                sort: *sort,
                limit: *limit,
                format: *format,
                paths: *paths,
                null: *null,
                long: *long,
            };
            std::process::exit(run_search(&args.db, &options));
        }
        Command::Report {
            path, filters, top_files, by_type, by_directory, depth, stale, stale_by, duplicates,
            min_size, min_copies, min_wasted, all_paths, limit, format,
        } => {
            let options = ReportOptions {
                path: path.as_deref(),
                filters,
                top_files: *top_files,
                by_type: *by_type,
                by_directory: *by_directory,
                depth: *depth as usize,
                stale: stale.as_ref(),
                stale_by: *stale_by,
                duplicates: *duplicates,
                min_size: *min_size,
                min_copies: *min_copies,
                min_wasted: *min_wasted,
                all_paths: *all_paths,
                limit: *limit,
                format: *format,
            };
            std::process::exit(run_report(&args.db, &options));
        }
        Command::Export { path, filters, sort, format } => {
            std::process::exit(run_export(&args.db, path.as_deref(), filters, *sort, (*format).into()));
        }
    };

    // Absolute, symlink-free root, so every spelling of the same directory shares a table
    let path = match fs::canonicalize(&typed) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("error: path not found: {} ({})", typed, e);
            std::process::exit(1);
        }
    };

    let upsert = matches!(args.cmd, Command::Update { .. });

    // Changes nothing, so it isn't logged
    if dry_run {
        std::process::exit(run_dry_run(&args.db, &typed, &path, upsert, filter.as_ref()));
    }

    let db_path = args.db.clone();
    let threads = args.threads;

    // Use plain-text mode if --no-tui is set OR stdout is not an interactive terminal
    // (e.g. cron job, SSH without TTY, piped output)
    let plain = args.no_tui || !std::io::stdout().is_terminal();

    runlog.start();

    // Start worker on a background thread
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    let path_w = path.clone();
    let db_w = db_path.clone();
    std::thread::spawn(move || run_worker(path_w, typed, db_w, threads, upsert, filter, tx));

    let mut state = AppState::new(mode, &path, &db_path);

    if plain {
        run_plain(&mut state, rx, &mut runlog);
    } else {
        // Initialise TUI terminal
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

        let result = run_ui(&mut terminal, &mut state, rx, &mut runlog);

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
            runlog.end("failed", &format!("UI error: {}", e), 1);
            report_log_failure(&runlog);
            std::process::exit(1);
        }

        // Print plain-text summary after TUI exits
        println!("\ndiskUnder — complete");
        println!("  files found   : {}", state.files_found);
        println!("  skipped       : {} ({})", state.files_skipped, state.skipped_label());
        println!("  files written : {}", state.files_written);
        println!("  rows removed  : {}", state.files_removed);
        if let Some(excluded) = state.excluded_line() {
            println!("  excluded      : {}", excluded);
        }
        println!("  errors        : {}", state.errors_summary());
        println!("  dup groups    : {}", state.dup_keys.len());
        if !state.dup_keys.is_empty() {
            println!("  wasted        : {}", fmt_bytes(state.dup_wasted));
        }
    }

    // A run that stopped on a fatal error exits non-zero, so cron and scripts can tell
    let (outcome, code) = match (&state.fatal, state.done) {
        (Some(_), _) => ("failed", 1),
        (None, true) => ("completed", 0),
        (None, false) => ("stopped by user", 0),
    };
    runlog.end(outcome, &state.log_summary(), code);
    report_log_failure(&runlog);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn report_log_failure(runlog: &RunLog) {
    if let Some(reason) = runlog.failure() {
        eprintln!("warning: {} — the run itself was not affected", reason);
    }
}
