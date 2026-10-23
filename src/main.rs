// diskUnder — file indexer with live TUI progress, duplicate detection and database maintenance
// See changes.md for full change history

mod cli;
mod db;
mod dryrun;
mod filters;
mod fmt;
mod jobs;
mod maintenance;
mod output;
mod plain;
mod progress;
mod query;
mod report;
mod run;
mod runlog;
mod scan;
mod search;
mod tui;

use clap::Parser;
use std::fs;

use crate::cli::{Cli, Command, JobsAction, Mode};
use crate::dryrun::run_dry_run;
use crate::jobs::{run_jobs_command, store_path, JobSettings, JobsCommand, LogArgs};
use crate::maintenance::run_db_command;
use crate::report::{run_report, ReportOptions};
use crate::run::{report_log_failure, run_indexing, IndexRun};
use crate::runlog::RunLog;
use crate::search::{run_export, run_search, SearchOptions};

fn main() {
    let args = Cli::parse();
    let db = args.db();
    let log_args = LogArgs { log: args.log.clone(), no_log: args.no_log, no_tui: args.no_tui };
    let store = store_path(args.config_db.as_deref());

    let code = match &args.cmd {
        Command::Index { path, dry_run, types } => index(&args, &db, false, path, *dry_run, types.overrides()),
        Command::Update { path, dry_run, types } => index(&args, &db, true, path, *dry_run, types.overrides()),
        Command::Jobs { action } => {
            let cmd = match action {
                JobsAction::Add { name, mode, path, types } => {
                    JobsCommand::Add { name, settings: job_settings(&args, Some(*mode), Some(path), types) }
                }
                JobsAction::Edit { name, mode, path, types } => {
                    JobsCommand::Edit { name, settings: job_settings(&args, *mode, path.as_deref(), types) }
                }
                JobsAction::List => JobsCommand::List,
                JobsAction::Show { name } => JobsCommand::Show { name },
                JobsAction::Remove { name, yes, drop_index } => {
                    JobsCommand::Remove { name, yes: *yes, drop_index: *drop_index }
                }
            };
            run_jobs_command(&store, cmd, &log_args)
        }
        Command::Run { name, dry_run } => {
            if args.db.is_some() {
                eprintln!("error: a job always uses its own database — change it with `diskunder jobs edit NAME --db FILE`");
                2
            } else {
                let cmd = JobsCommand::Run { name: name.as_deref(), dry_run: *dry_run, threads: args.threads };
                run_jobs_command(&store, cmd, &log_args)
            }
        }
        Command::Db { action } => {
            let mut runlog = RunLog::new(&db, args.log.as_deref(), args.no_log);
            let code = run_db_command(&db, action, &mut runlog);
            report_log_failure(&runlog);
            code
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
            run_search(&db, &options)
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
            run_report(&db, &options)
        }
        Command::Export { path, filters, sort, format } => {
            run_export(&db, path.as_deref(), filters, *sort, (*format).into())
        }
    };
    std::process::exit(code);
}

/// Job settings from the command line: only what was given (the global --db and --threads
/// count only when passed)
fn job_settings<'a>(args: &'a Cli, mode: Option<Mode>, path: Option<&'a str>, types: &cli::TypeArgs) -> JobSettings<'a> {
    JobSettings {
        upsert: mode.map(|m| matches!(m, Mode::Update)),
        path,
        db: args.db.as_deref(),
        threads: args.threads,
        overrides: types.overrides(),
    }
}

/// `index` / `update`, or their dry runs
fn index(args: &Cli, db: &str, upsert: bool, typed: &str, dry_run: bool, overrides: filters::Overrides) -> i32 {
    if dry_run {
        // Changes nothing, so it isn't logged
        return match fs::canonicalize(typed) {
            Ok(root) => run_dry_run(db, typed, &root.to_string_lossy(), upsert, &overrides),
            Err(e) => {
                eprintln!("error: path not found: {} ({})", typed, e);
                1
            }
        };
    }
    let mut runlog = RunLog::new(db, args.log.as_deref(), args.no_log);
    let run = IndexRun {
        upsert,
        typed: typed.to_string(),
        db: db.to_string(),
        threads: args.threads(),
        overrides,
        no_tui: args.no_tui,
    };
    let outcome = run_indexing(run, &mut runlog);
    report_log_failure(&runlog);
    outcome.code
}
