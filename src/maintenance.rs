//! The `db` subcommand: info, drop, vacuum, check

use rusqlite::{Connection, Result as DbResult};
use std::{
    fs,
    io::IsTerminal,
    path::Path,
};

use crate::cli::DbAction;
use crate::db::{db_file_bytes, file_tables, load_roots, table_disk_bytes, table_duplicates, table_exists, table_name_for};
use crate::fmt::{fmt_bytes, fmt_count, fmt_local};
use crate::runlog::RunLog;

fn db_info(conn: &Connection, db_path: &str) -> DbResult<()> {
    let tables = file_tables(conn)?;
    let roots = load_roots(conn)?;
    println!("database: {} — {} on disk", db_path, fmt_bytes(db_file_bytes(db_path)));
    if tables.is_empty() {
        println!("\nno indexed paths yet");
    }

    // Recorded paths first (alphabetical), then tables with no recorded path
    let mut ordered: Vec<&String> = tables.iter().collect();
    ordered.sort_by_key(|t| (roots.get(*t).is_none(), roots.get(*t).map(|r| r.root.clone())));

    for t in ordered {
        let (files, bytes): (i64, i64) = conn.query_row(
            &format!("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM {t}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (groups, wasted) = table_duplicates(conn, t)?;

        println!();
        match roots.get(t) {
            Some(r) => println!("{}", r.root),
            None => println!("(no recorded path)"),
        }
        println!("  files       {} ({})", fmt_count(files), fmt_bytes(bytes as u64));
        println!(
            "  duplicates  {} group{}, {} wasted",
            fmt_count(groups),
            if groups == 1 { "" } else { "s" },
            fmt_bytes(wasted as u64)
        );
        match roots.get(t) {
            Some(r) => {
                let outcome = match &r.last_finished {
                    Some(end) => format!("finished {}", fmt_local(end)),
                    None => "did not finish (interrupted, or still running)".to_string(),
                };
                println!("  last run    {} at {}, {}", r.last_mode, fmt_local(&r.last_started), outcome);
                println!("  first run   {}", fmt_local(&r.first_run));
                if !r.filter.types.is_empty() {
                    println!("  filters     {}", r.filter.types.describe());
                }
                if !r.filter.paths.is_empty() {
                    println!("  paths       {}", r.filter.paths.describe());
                }
            }
            None => println!(
                "  note        from before v4.1, or a path spelled differently in v3 — runs of its\n              \
                 path will record it; if unwanted, remove with `diskunder db drop --table {}`",
                t
            ),
        }
        match table_disk_bytes(conn, t) {
            Some(b) => println!("  table       {} ({} with indexes)", t, fmt_bytes(b as u64)),
            None => println!("  table       {}", t),
        }
    }
    Ok(())
}

struct DropTarget {
    table: String,
    label: String, // the indexed path, or the table name if none is recorded
    files: i64,
}

/// Find the table to drop. Nothing has changed if this fails, so it isn't logged.
fn resolve_drop(conn: &Connection, path: Option<&str>, table: Option<&str>) -> Result<DropTarget, String> {
    let db = |e: rusqlite::Error| e.to_string();
    let roots = load_roots(conn).map_err(db)?;

    let (t, label) = match (path, table) {
        (_, Some(t)) => {
            if !t.starts_with("files_") || !table_exists(conn, t).map_err(db)? {
                return Err(format!("no index table named {}", t));
            }
            let label = roots.get(t).map(|r| r.root.clone()).unwrap_or_else(|| t.to_string());
            (t.to_string(), label)
        }
        (Some(p), None) => {
            // The path may no longer exist (e.g. a retired share): fall back to making it
            // absolute without touching the filesystem
            let root = fs::canonicalize(p)
                .or_else(|_| std::path::absolute(p))
                .map(|c| match c.to_string_lossy().trim_end_matches('/') {
                    "" => "/".to_string(),
                    trimmed => trimmed.to_string(),
                })
                .unwrap_or_else(|_| p.to_string());
            let t = roots
                .iter()
                .find(|(_, r)| r.root == root)
                .map(|(t, _)| t.clone())
                .unwrap_or_else(|| table_name_for(&root));
            if !table_exists(conn, &t).map_err(db)? {
                return Err(format!("no index for {} — see `diskunder db info`", root));
            }
            (t, root)
        }
        (None, None) => unreachable!("clap requires a path or --table"),
    };

    let files: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |row| row.get(0))
        .map_err(db)?;
    Ok(DropTarget { table: t, label, files })
}

fn confirm_drop(target: &DropTarget, yes: bool) -> Result<(), String> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("not running interactively — pass --yes to confirm".to_string());
        }
        eprint!(
            "Remove the index of {} ({} files)? This can't be undone. [y/N] ",
            target.label,
            fmt_count(target.files)
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("cancelled".to_string());
        }
    }
    Ok(())
}

/// Returns the summary line, which is both printed and logged
fn perform_drop(conn: &Connection, target: &DropTarget) -> Result<String, String> {
    let db = |e: rusqlite::Error| e.to_string();
    conn.execute(&format!("DROP TABLE {}", target.table), []).map_err(db)?;
    if table_exists(conn, "roots").map_err(db)? {
        conn.execute("DELETE FROM roots WHERE table_name = ?1", [&target.table]).map_err(db)?;
    }
    Ok(format!("removed the index of {} ({} files)", target.label, fmt_count(target.files)))
}

fn db_drop(
    conn: &Connection,
    path: Option<&str>,
    table: Option<&str>,
    yes: bool,
    runlog: &mut RunLog,
) -> Result<(), String> {
    let target = resolve_drop(conn, path, table)?;
    confirm_drop(&target, yes)?;
    let summary = logged(runlog, || perform_drop(conn, &target))?;
    println!("{}", summary);
    println!("run `diskunder db vacuum` to shrink the database file");
    Ok(())
}

/// Run a database-changing action between `start` and `end` run-log lines
fn logged(runlog: &mut RunLog, action: impl FnOnce() -> Result<String, String>) -> Result<String, String> {
    runlog.start();
    let result = action();
    match &result {
        Ok(summary) => runlog.end("completed", summary, 0),
        Err(e) => {
            runlog.error(e);
            runlog.end("failed", e, 1);
        }
    }
    result
}

/// Returns the summary line, which is both printed and logged
fn perform_vacuum(conn: &Connection, db_path: &str) -> DbResult<String> {
    let before = db_file_bytes(db_path);
    conn.execute_batch("VACUUM;")?;
    // Fold the WAL back in and truncate it, so the size reported is the real footprint
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let after = db_file_bytes(db_path);
    Ok(format!("vacuumed {}: {} → {}", db_path, fmt_bytes(before), fmt_bytes(after)))
}

/// Returns false if SQLite reports corruption
fn db_check(conn: &Connection) -> DbResult<bool> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let problems: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<DbResult<Vec<String>>>()?
        .into_iter()
        .filter(|line| line != "ok")
        .collect();
    if problems.is_empty() {
        println!("integrity: ok");
    } else {
        println!("integrity: {} problem(s)", problems.len());
        for p in &problems {
            println!("  {}", p);
        }
    }

    let tables = file_tables(conn)?;
    let roots = load_roots(conn)?;
    let unrecorded: Vec<&String> = tables.iter().filter(|t| !roots.contains_key(*t)).collect();
    let missing: Vec<&String> = roots.keys().filter(|t| !tables.contains(t)).collect();
    if !unrecorded.is_empty() {
        println!("tables with no recorded path (see `diskunder db info`):");
        for t in unrecorded {
            println!("  {}", t);
        }
    }
    if !missing.is_empty() {
        println!("recorded paths whose table is missing:");
        for t in missing {
            println!("  {} ({})", roots[t].root, t);
        }
    }
    Ok(problems.is_empty())
}

/// Runs a `db` action; returns the process exit code
/// Runs a `db` action; returns the process exit code. Only `drop` and `vacuum` change the
/// database, so only they are logged.
pub(crate) fn run_db_command(db_path: &str, action: &DbAction, runlog: &mut RunLog) -> i32 {
    // Inspecting a database shouldn't create one
    if !Path::new(db_path).exists() {
        eprintln!("error: no database at {}", db_path);
        return 1;
    }
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}: {}", db_path, e);
            return 1;
        }
    };
    let result = match action {
        DbAction::Info => db_info(&conn, db_path).map_err(|e| e.to_string()),
        DbAction::Drop { path, table, yes } => {
            db_drop(&conn, path.as_deref(), table.as_deref(), *yes, runlog)
        }
        DbAction::Vacuum => logged(runlog, || perform_vacuum(&conn, db_path).map_err(|e| e.to_string()))
            .map(|summary| println!("{}", summary)),
        DbAction::Check => match db_check(&conn) {
            Ok(true) => Ok(()),
            Ok(false) => return 2,
            Err(e) => Err(e.to_string()),
        },
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}
