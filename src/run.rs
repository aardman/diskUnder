//! One `index` or `update` run — worker thread, progress display (TUI or plain), run log —
//! shared by the `index` / `update` commands and by `run NAME` for jobs

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{fs, io::IsTerminal, sync::mpsc};

use crate::filters::Overrides;
use crate::fmt::fmt_bytes;
use crate::plain::run_plain;
use crate::progress::AppState;
use crate::runlog::RunLog;
use crate::scan::{run_worker, WorkerMsg};
use crate::tui::run_ui;

pub(crate) struct IndexRun {
    pub(crate) upsert: bool, // update rather than index
    pub(crate) typed: String, // the path as given
    pub(crate) db: String,
    pub(crate) threads: usize,
    pub(crate) overrides: Overrides,
    pub(crate) no_tui: bool,
    pub(crate) hash_all: bool, // checksum every file, not just possible copies
}

/// The counts a run ends with, as shown in its summary
#[derive(Default)]
pub(crate) struct RunCounts {
    pub(crate) found: usize,
    pub(crate) skipped: usize,
    pub(crate) written: usize,
    pub(crate) removed: usize,
    pub(crate) errors_unreadable: usize,
    pub(crate) errors_unwritten: usize,
    pub(crate) errors_other: usize,
    pub(crate) excluded_files: usize,
    pub(crate) excluded_folders: usize,
    pub(crate) rows_excluded_removed: usize,
    pub(crate) dup_groups: usize,
    pub(crate) wasted: u64,
}

impl RunCounts {
    fn from_state(s: &AppState) -> Self {
        RunCounts {
            found: s.files_found,
            skipped: s.files_skipped,
            written: s.files_written,
            removed: s.files_removed,
            errors_unreadable: s.files_errored,
            errors_unwritten: s.files_unwritten,
            errors_other: s.other_errors,
            excluded_files: s.files_excluded,
            excluded_folders: s.folders_excluded,
            rows_excluded_removed: s.rows_excluded_removed,
            dup_groups: s.dup_keys.len(),
            wasted: s.dup_wasted,
        }
    }
}

pub(crate) struct RunOutcome {
    pub(crate) code: i32,
    pub(crate) outcome: &'static str, // completed | failed | stopped by user
    pub(crate) summary: String,       // the run log's one-line summary, or the error
    pub(crate) counts: RunCounts,
}

impl RunOutcome {
    pub(crate) fn failed(summary: String) -> Self {
        RunOutcome { code: 1, outcome: "failed", summary, counts: RunCounts::default() }
    }
}

/// A run in progress: its messages arrive on `rx` and are applied to `state`
pub(crate) struct Started {
    pub(crate) rx: mpsc::Receiver<WorkerMsg>,
    pub(crate) state: AppState,
}

/// Start `index` or `update` on a worker thread, logging to `runlog`. `command` is what the
/// run log records as the command (None: this process's own command line). Fails, without
/// printing, if the run can't start.
pub(crate) fn start_run(r: &IndexRun, runlog: &mut RunLog, command: Option<String>) -> Result<Started, RunOutcome> {
    // Absolute, symlink-free root, so every spelling of the same directory shares a table
    let path = fs::canonicalize(&r.typed)
        .map_err(|e| RunOutcome::failed(format!("path not found: {} ({})", r.typed, e)))?
        .to_string_lossy()
        .into_owned();
    let mode = if r.upsert { "update" } else { "index" };

    // The database is created if absent — including its folder
    if let Some(dir) = std::path::Path::new(&r.db).parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| {
            RunOutcome::failed(format!("can't create the database folder {}: {}", dir.display(), e))
        })?;
    }

    // One writer per database; held by the worker until the run ends
    let what = command.clone().unwrap_or_else(|| std::env::args().skip(1).collect::<Vec<_>>().join(" "));
    let lock = crate::db::lock_db(&r.db, &format!("diskunder {}", what)).map_err(RunOutcome::failed)?;

    match command {
        Some(c) => runlog.start_as(&c),
        None => runlog.start(),
    }
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    {
        let (path, typed, db) = (path.clone(), r.typed.clone(), r.db.clone());
        let (threads, upsert, hash_all, overrides) = (r.threads, r.upsert, r.hash_all, r.overrides.clone());
        std::thread::spawn(move || {
            let _lock = lock;
            run_worker(path, typed, db, threads, upsert, hash_all, overrides, tx)
        });
    }
    Ok(Started { rx, state: AppState::new(mode, &path, &r.db) })
}

/// The outcome of a run whose messages have all been applied (or that was stopped early),
/// written to the run log
pub(crate) fn finish_run(state: &AppState, runlog: &mut RunLog) -> RunOutcome {
    // A run that stopped on a fatal error exits non-zero, so cron and scripts can tell
    let (outcome, code) = match (&state.fatal, state.done) {
        (Some(_), _) => ("failed", 1),
        (None, true) => ("completed", 0),
        (None, false) => ("stopped by user", 0),
    };
    let summary = state.log_summary();
    runlog.end(outcome, &summary, code);
    RunOutcome { code, outcome, summary, counts: RunCounts::from_state(state) }
}

/// Run `index` or `update` to completion with the TUI or plain output, logging to `runlog`.
/// Errors that stop the run are printed here.
pub(crate) fn run_indexing(r: IndexRun, runlog: &mut RunLog) -> RunOutcome {
    let Started { rx, mut state } = match start_run(&r, runlog, None) {
        Ok(s) => s,
        Err(failed) => {
            eprintln!("error: {}", failed.summary);
            return failed;
        }
    };

    // Plain text if --no-tui is set or stdout isn't an interactive terminal (cron, SSH, pipes)
    let plain = r.no_tui || !std::io::stdout().is_terminal();
    if plain {
        run_plain(&mut state, rx, runlog);
    } else if let Err(e) = tui(&mut state, rx, runlog) {
        eprintln!("UI error: {}", e);
        let msg = format!("UI error: {}", e);
        runlog.end("failed", &msg, 1);
        return RunOutcome::failed(msg);
    }
    finish_run(&state, runlog)
}

fn tui(state: &mut AppState, rx: mpsc::Receiver<WorkerMsg>, runlog: &mut RunLog) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run_ui(&mut terminal, state, rx, runlog);

    // Always restore the terminal, even on error
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result?;

    // Plain-text summary once the TUI has closed
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
    Ok(())
}

pub(crate) fn report_log_failure(runlog: &RunLog) {
    if let Some(reason) = runlog.failure() {
        eprintln!("warning: {} — the run itself was not affected", reason);
    }
}
