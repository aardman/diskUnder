//! The `db` subcommand: info, drop, vacuum, check

use rusqlite::{Connection, Result as DbResult};
use std::{
    fs,
    io::IsTerminal,
    path::Path,
};

use crate::db::lock_db;
use crate::cli::DbAction;
use crate::db::{db_file_bytes, file_tables, load_roots, table_disk_bytes, table_duplicates, table_exists, table_name_for};
use crate::fmt::{fmt_bytes, fmt_count, fmt_local};
use crate::output::Out;
use crate::runlog::RunLog;

/// Like println!, into an Out (stdout, or a buffer for the app)
macro_rules! w {
    ($out:expr, $($arg:tt)*) => {{
        $out.write(&format!($($arg)*));
        $out.write("\n");
    }};
}

fn db_info(conn: &Connection, db_path: &str, out: &mut Out, sizes: bool) -> DbResult<()> {
    let tables = file_tables(conn)?;
    let roots = load_roots(conn)?;
    w!(out, "database: {} — {} on disk", db_path, fmt_bytes(db_file_bytes(db_path)));
    if tables.is_empty() {
        w!(out, "\nno indexed paths yet");
    }
    // With the duplicate summary (5.7+): one line for the whole database, instead of grouping
    // each path's files — minutes on tens of millions of rows
    let summary = crate::db::has_dup_groups(conn)?;
    if summary {
        let (groups, wasted): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size * (copies - 1)), 0) FROM dup_groups",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        w!(out, "duplicates: {} group{}, {} wasted, across all paths", fmt_count(groups), if groups == 1 { "" } else { "s" }, fmt_bytes(wasted as u64));
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
        w!(out, "");
        match roots.get(t) {
            Some(r) => w!(out, "{}", r.root),
            None => w!(out, "(no recorded path)"),
        }
        w!(out, "  files       {} ({})", fmt_count(files), fmt_bytes(bytes as u64));
        if summary {
            let hashed: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {t} WHERE checksum IS NOT NULL"), [], |r| r.get(0))?;
            w!(out, "  hashed      {} — the rest have no possible copy", fmt_count(hashed));
        } else {
            let (groups, wasted) = table_duplicates(conn, t)?;
            w!(out,
                "  duplicates  {} group{}, {} wasted",
                fmt_count(groups),
                if groups == 1 { "" } else { "s" },
                fmt_bytes(wasted as u64)
            );
        }
        match roots.get(t) {
            Some(r) => {
                let outcome = match &r.last_finished {
                    Some(end) => format!("finished {}", fmt_local(end)),
                    None => "did not finish (interrupted, or still running)".to_string(),
                };
                w!(out, "  last run    {} at {}, {}", r.last_mode, fmt_local(&r.last_started), outcome);
                w!(out, "  first run   {}", fmt_local(&r.first_run));
                if !r.filter.types.is_empty() {
                    w!(out, "  filters     {}", r.filter.types.describe());
                }
                if !r.filter.paths.is_empty() {
                    w!(out, "  paths       {}", r.filter.paths.describe());
                }
                if let Some(v) = &r.volume {
                    w!(out, "  volume      {}  ({})", v.id, v.path);
                    for (other, o) in &roots {
                        if other.as_str() != t.as_str() && o.volume.as_ref().is_some_and(|ov| crate::volume::overlaps(ov, v)) {
                            w!(out, "  overlaps    {} — same files; duplicates count them once", o.root);
                        }
                    }
                }
            }
            None => w!(out, 
                "  note        from before v4.1, or a path spelled differently in v3 — runs of its\n              \
                 path will record it; if unwanted, remove with `diskunder db drop --table {}`",
                t
            ),
        }
        match if sizes { table_disk_bytes(conn, t) } else { None } {
            Some(b) => w!(out, "  table       {} ({} with indexes)", t, fmt_bytes(b as u64)),
            None => w!(out, "  table       {}", t),
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
    // Its files leave the duplicate summary with it
    let checksums = if crate::db::has_dup_groups(conn).map_err(db)? {
        crate::db::table_checksums(conn, &target.table).map_err(db)?
    } else {
        Vec::new()
    };
    conn.execute(&format!("DROP TABLE {}", target.table), []).map_err(db)?;
    if table_exists(conn, "roots").map_err(db)? {
        conn.execute("DELETE FROM roots WHERE table_name = ?1", [&target.table]).map_err(db)?;
    }
    crate::db::refresh_dup_groups(conn, &checksums).map_err(db)?;
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

/// Remove an index table by name without asking (the caller has confirmed); logged
pub(crate) fn drop_table(db_path: &str, table: &str, runlog: &mut RunLog) -> Result<String, String> {
    let conn = open_existing(db_path)?;
    let target = resolve_drop(&conn, None, Some(table))?;
    let _lock = lock_db(db_path, "diskunder db drop [app]")?;
    logged(runlog, || perform_drop(&conn, &target))
}

/// Remove the index of `root` from `db_path` without asking (the caller has confirmed);
/// logged like `db drop`. Used by `jobs remove --drop-index`.
pub(crate) fn drop_index(db_path: &str, root: &str, runlog: &mut RunLog) -> Result<String, String> {
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    let target = resolve_drop(&conn, Some(root), None)?;
    let _lock = lock_db(db_path, "diskunder jobs remove --drop-index")?;
    logged(runlog, || perform_drop(&conn, &target))
}

fn open_existing(db_path: &str) -> Result<Connection, String> {
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))
}

/// `db info` as text — for the app
pub(crate) fn info_text(db_path: &str) -> Result<String, String> {
    let conn = open_existing(db_path)?;
    let mut out = Out::buffer();
    db_info(&conn, db_path, &mut out, false).map_err(|e| e.to_string())?;
    Ok(out.into_text())
}

/// `db check` as text, and whether the integrity check passed — for the app
pub(crate) fn check_text(db_path: &str) -> Result<(String, bool), String> {
    let conn = open_existing(db_path)?;
    let mut out = Out::buffer();
    let ok = db_check(&conn, &mut out).map_err(|e| e.to_string())?;
    Ok((out.into_text(), ok))
}

/// `db vacuum`, logged — for the app
pub(crate) fn vacuum(db_path: &str, runlog: &mut RunLog) -> Result<String, String> {
    let conn = open_existing(db_path)?;
    let _lock = lock_db(db_path, "diskunder db vacuum [app]")?;
    logged(runlog, || perform_vacuum(&conn, db_path).map_err(|e| e.to_string()))
}

/// A consistent copy of the database at `dest` (SQLite's VACUUM INTO — safe while other
/// processes read it, and compacted). Changes nothing, so it isn't logged.
pub(crate) fn backup(db_path: &str, dest: &str) -> Result<String, String> {
    let conn = open_existing(db_path)?;
    if Path::new(dest).exists() {
        return Err(format!("{} already exists — choose a new file name", dest));
    }
    if let Some(dir) = Path::new(dest).parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {}", dir.display(), e))?;
    }
    conn.execute("VACUUM INTO ?1", [dest]).map_err(|e| e.to_string())?;
    let size = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    Ok(format!("backed up {} to {} ({})", db_path, dest, fmt_bytes(size)))
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
fn db_check(conn: &Connection, out: &mut Out) -> DbResult<bool> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let problems: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<DbResult<Vec<String>>>()?
        .into_iter()
        .filter(|line| line != "ok")
        .collect();
    if problems.is_empty() {
        w!(out, "integrity: ok");
    } else {
        w!(out, "integrity: {} problem(s)", problems.len());
        for p in &problems {
            w!(out, "  {}", p);
        }
    }

    let tables = file_tables(conn)?;
    let roots = load_roots(conn)?;
    let unrecorded: Vec<&String> = tables.iter().filter(|t| !roots.contains_key(*t)).collect();
    let missing: Vec<&String> = roots.keys().filter(|t| !tables.contains(t)).collect();
    if !unrecorded.is_empty() {
        w!(out, "tables with no recorded path (see `diskunder db info`):");
        for t in unrecorded {
            w!(out, "  {}", t);
        }
    }
    if !missing.is_empty() {
        w!(out, "recorded paths whose table is missing:");
        for t in missing {
            w!(out, "  {} ({})", roots[t].root, t);
        }
    }
    Ok(problems.is_empty())
}

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
    let mut out = Out::new();
    let result = match action {
        DbAction::Info { sizes } => db_info(&conn, db_path, &mut out, *sizes).map_err(|e| e.to_string()),
        DbAction::Drop { path, table, yes } => lock_db(db_path, "diskunder db drop")
            .and_then(|_lock| db_drop(&conn, path.as_deref(), table.as_deref(), *yes, runlog)),
        DbAction::Vacuum => lock_db(db_path, "diskunder db vacuum").and_then(|_lock| {
            logged(runlog, || perform_vacuum(&conn, db_path).map_err(|e| e.to_string())).map(|summary| println!("{}", summary))
        }),
        DbAction::Check => match db_check(&conn, &mut out) {
            Ok(true) => Ok(()),
            Ok(false) => {
                out.flush();
                return 2;
            }
            Err(e) => Err(e.to_string()),
        },
        DbAction::Backup { dest } => backup(db_path, dest).map(|summary| println!("{}", summary)),
    };
    out.flush();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}
