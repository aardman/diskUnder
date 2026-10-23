//! Jobs: named, saved `index` / `update` runs with their run history, kept in a per-user
//! store (not an index database, since each job names its own)

use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs::{self, File, TryLockError},
    io::IsTerminal,
    path::{Path, PathBuf},
};

use crate::db::table_exists;
use crate::dryrun::run_dry_run;
use crate::filters::{IndexFilter, Overrides, PathFilter, TypeFilter};
use crate::fmt::{fmt_count, fmt_local};
use crate::maintenance::drop_index;
use crate::output::{write_text_table, Align, Out};
use crate::query::absolute;
use crate::run::{report_log_failure, run_indexing, IndexRun};
use crate::runlog::RunLog;

const STORE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    name          TEXT PRIMARY KEY,
    mode          TEXT NOT NULL,      -- index | update
    path          TEXT NOT NULL,      -- absolute
    db            TEXT NOT NULL,      -- absolute
    threads       INTEGER NOT NULL,
    include_ext   TEXT,
    exclude_ext   TEXT,
    exclude_paths TEXT,
    include_paths TEXT,
    created       TEXT NOT NULL,      -- UTC
    updated       TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS job_runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    job           TEXT NOT NULL,
    run_id        TEXT NOT NULL,      -- matches run=… in the index database's run log
    started       TEXT NOT NULL,      -- UTC
    finished      TEXT,               -- NULL while running, or if the run was killed
    outcome       TEXT NOT NULL,      -- running | completed | failed | stopped by user
    exit_code     INTEGER,
    duration_secs INTEGER,
    found INTEGER, skipped INTEGER, written INTEGER, removed INTEGER,
    errors_unreadable INTEGER, errors_unwritten INTEGER, errors_other INTEGER,
    excluded_files INTEGER, excluded_folders INTEGER, rows_excluded_removed INTEGER,
    dup_groups INTEGER, wasted INTEGER,
    summary       TEXT,
    errors_dropped INTEGER            -- errors beyond the 50 kept in job_run_errors
);
CREATE INDEX IF NOT EXISTS idx_job_runs_job ON job_runs(job, id);
CREATE TABLE IF NOT EXISTS job_run_errors (
    run           INTEGER NOT NULL,   -- job_runs.id
    seq           INTEGER NOT NULL,
    message       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_job_run_errors_run ON job_run_errors(run, seq);
";

// ─── The store ───────────────────────────────────────────────────────────────

/// --config-db, else $DISKUNDER_CONFIG_DB, else $XDG_CONFIG_HOME/diskunder/diskunder.db,
/// else ~/.config/diskunder/diskunder.db
pub(crate) fn store_path(arg: Option<&str>) -> PathBuf {
    if let Some(p) = arg {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("DISKUNDER_CONFIG_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"));
    base.join("diskunder").join("diskunder.db")
}

/// Open the store. Commands that only read it don't create it.
fn open_store(path: &Path, create: bool) -> Result<Option<Connection>, String> {
    if !path.exists() {
        if !create {
            return Ok(None);
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {}", dir.display(), e))?;
        }
    }
    let conn = Connection::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if !create && !table_exists(&conn, "jobs").map_err(|e| e.to_string())? {
        return Ok(None);
    }
    conn.execute_batch(STORE_SCHEMA).map_err(|e| e.to_string())?;
    Ok(Some(conn))
}

fn lock_path(store: &Path, name: &str) -> PathBuf {
    store.with_file_name("locks").join(format!("{}.lock", name))
}

/// Whether another process holds the job's lock right now
fn is_running(store: &Path, name: &str) -> bool {
    File::open(lock_path(store, name)).is_ok_and(|f| matches!(f.try_lock(), Err(TryLockError::WouldBlock)))
}

// ─── Jobs ────────────────────────────────────────────────────────────────────

pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) upsert: bool,
    pub(crate) path: String,
    pub(crate) db: String,
    pub(crate) threads: usize,
    pub(crate) filter: IndexFilter,
}

impl Job {
    fn mode(&self) -> &'static str {
        if self.upsert { "update" } else { "index" }
    }

    fn filters_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.filter.types.is_empty() {
            parts.push(self.filter.types.describe());
        }
        if !self.filter.paths.is_empty() {
            parts.push(self.filter.paths.describe());
        }
        if parts.is_empty() { "none".to_string() } else { parts.join(" · ") }
    }
}

const JOB_COLUMNS: &str = "name, mode, path, db, threads, include_ext, exclude_ext, exclude_paths, include_paths";

fn job_from_row(r: &rusqlite::Row) -> rusqlite::Result<Job> {
    Ok(Job {
        name: r.get(0)?,
        upsert: r.get::<_, String>(1)? == "update",
        path: r.get(2)?,
        db: r.get(3)?,
        threads: r.get::<_, i64>(4)? as usize,
        filter: IndexFilter {
            types: TypeFilter::from_db(r.get(5)?, r.get(6)?),
            paths: PathFilter::from_db(r.get(7)?, r.get(8)?),
        },
    })
}

fn load_job(conn: &Connection, name: &str) -> Result<Option<Job>, String> {
    conn.query_row(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE name = ?1"), [name], job_from_row)
        .optional()
        .map_err(|e| e.to_string())
}

fn load_jobs(conn: &Connection) -> Result<Vec<Job>, String> {
    let mut stmt = conn.prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY name")).map_err(|e| e.to_string())?;
    let jobs = stmt.query_map([], job_from_row).map_err(|e| e.to_string())?.collect::<rusqlite::Result<_>>();
    jobs.map_err(|e| e.to_string())
}

fn save_job(conn: &Connection, j: &Job, new: bool) -> Result<(), String> {
    let (include_ext, exclude_ext) = j.filter.types.to_db();
    let (exclude_paths, include_paths) = j.filter.paths.to_db();
    let sql = if new {
        "INSERT INTO jobs (name, mode, path, db, threads, include_ext, exclude_ext, exclude_paths, include_paths, created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'), datetime('now'))"
    } else {
        "UPDATE jobs SET mode = ?2, path = ?3, db = ?4, threads = ?5, include_ext = ?6, exclude_ext = ?7,
                         exclude_paths = ?8, include_paths = ?9, updated = datetime('now')
         WHERE name = ?1"
    };
    conn.execute(
        sql,
        params![j.name, j.mode(), j.path, j.db, j.threads as i64, include_ext, exclude_ext, exclude_paths, include_paths],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Letters, digits, - and _, so names are safe in cron lines and file names
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(()) } else { Err(format!("job names use letters, digits, - and _ only (up to 64): '{}'", name)) }
}

/// Paths are stored absolute, so cron's working directory doesn't matter. The share may be
/// offline when a job is set up, so a missing path is allowed (with a warning).
fn absolute_path(p: &str, what: &str) -> String {
    match fs::canonicalize(p) {
        Ok(c) => c.to_string_lossy().into_owned(),
        Err(_) => {
            let abs = absolute(p);
            if what == "path" {
                eprintln!("warning: {} doesn't exist right now — saved anyway", abs);
            }
            abs
        }
    }
}

/// The database path as given (or the default), made absolute without requiring it to exist
fn absolute_db(db: &str) -> String {
    std::path::absolute(db).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| db.to_string())
}

// ─── Run history ─────────────────────────────────────────────────────────────

struct RunRow {
    id: i64,
    run_id: String,
    started: String,
    finished: Option<String>,
    outcome: String,
    exit_code: Option<i64>,
    duration: Option<i64>,
    found: Option<i64>,
    written: Option<i64>,
    removed: Option<i64>,
    errors: Option<i64>,
    summary: Option<String>,
}

const RUN_COLUMNS: &str = "id, run_id, started, finished, outcome, exit_code, duration_secs, found, written, removed,
     errors_unreadable + errors_unwritten + errors_other, summary";

fn run_from_row(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        run_id: r.get(1)?,
        started: r.get(2)?,
        finished: r.get(3)?,
        outcome: r.get(4)?,
        exit_code: r.get(5)?,
        duration: r.get(6)?,
        found: r.get(7)?,
        written: r.get(8)?,
        removed: r.get(9)?,
        errors: r.get(10)?,
        summary: r.get(11)?,
    })
}

fn recent_runs(conn: &Connection, job: &str, n: usize) -> Result<Vec<RunRow>, String> {
    let mut stmt = conn
        .prepare(&format!("SELECT {RUN_COLUMNS} FROM job_runs WHERE job = ?1 ORDER BY id DESC LIMIT {n}"))
        .map_err(|e| e.to_string())?;
    let rows = stmt.query_map([job], run_from_row).map_err(|e| e.to_string())?.collect::<rusqlite::Result<_>>();
    rows.map_err(|e| e.to_string())
}

/// "completed", "failed (exit 1)", "running", or "did not finish" for a run that was killed
fn status(r: &RunRow, running_now: bool) -> String {
    match r.outcome.as_str() {
        "running" if running_now => "running".to_string(),
        "running" => "did not finish".to_string(),
        "failed" => format!("failed (exit {})", r.exit_code.unwrap_or(1)),
        other => other.to_string(),
    }
}

/// 14m33s, 2h05m, 9s
fn fmt_secs(s: i64) -> String {
    match s {
        0..=59 => format!("{}s", s),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn opt_count(n: Option<i64>) -> String {
    n.map_or("—".to_string(), fmt_count)
}

// ─── Commands ────────────────────────────────────────────────────────────────

pub(crate) struct JobSettings<'a> {
    pub(crate) upsert: Option<bool>,
    pub(crate) path: Option<&'a str>,
    pub(crate) db: Option<&'a str>,
    pub(crate) threads: Option<usize>,
    pub(crate) overrides: Overrides,
}

fn add(store: &Path, name: &str, s: &JobSettings) -> Result<(), String> {
    check_name(name)?;
    let conn = open_store(store, true)?.expect("created");
    if load_job(&conn, name)?.is_some() {
        return Err(format!("job {} already exists — use `diskunder jobs edit {}`", name, name));
    }
    let job = Job {
        name: name.to_string(),
        upsert: s.upsert.unwrap_or(false),
        path: absolute_path(s.path.expect("clap requires PATH"), "path"),
        db: absolute_db(s.db.unwrap_or("fileindex.db")),
        threads: s.threads.unwrap_or(4),
        filter: s.overrides.apply(&IndexFilter::default()),
    };
    save_job(&conn, &job, true)?;
    println!("added job {}: {} {} → {}", job.name, job.mode(), job.path, job.db);
    println!("  threads {} · filters {}", job.threads, job.filters_line());
    println!("run it with: diskunder run {}", job.name);
    Ok(())
}

fn edit(store: &Path, name: &str, s: &JobSettings) -> Result<(), String> {
    let conn = open_store(store, false)?.ok_or_else(|| no_such_job(name))?;
    let mut job = load_job(&conn, name)?.ok_or_else(|| no_such_job(name))?;
    let before = (job.mode(), job.path.clone(), job.db.clone(), job.threads, job.filters_line());
    if let Some(u) = s.upsert {
        job.upsert = u;
    }
    if let Some(p) = s.path {
        job.path = absolute_path(p, "path");
    }
    if let Some(d) = s.db {
        job.db = absolute_db(d);
    }
    if let Some(t) = s.threads {
        job.threads = t;
    }
    job.filter = s.overrides.apply(&job.filter);
    let after = (job.mode(), job.path.clone(), job.db.clone(), job.threads, job.filters_line());
    if before == after {
        println!("job {} unchanged", name);
        return Ok(());
    }
    save_job(&conn, &job, false)?;
    println!("updated job {}:", name);
    let changes = [
        ("mode", before.0.to_string(), after.0.to_string()),
        ("path", before.1, after.1),
        ("database", before.2, after.2),
        ("threads", before.3.to_string(), after.3.to_string()),
        ("filters", before.4, after.4),
    ];
    for (field, old, new) in changes {
        if old != new {
            println!("  {:<9} {} → {}", field, old, new);
        }
    }
    Ok(())
}

fn no_such_job(name: &str) -> String {
    format!("no job named {} — see `diskunder jobs list`", name)
}

fn confirm(question: &str) -> Result<bool, String> {
    eprint!("{} [y/N] ", question);
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn remove(store: &Path, name: &str, yes: bool, drop: bool, args: &LogArgs) -> Result<(), String> {
    let conn = open_store(store, false)?.ok_or_else(|| no_such_job(name))?;
    let job = load_job(&conn, name)?.ok_or_else(|| no_such_job(name))?;
    if is_running(store, name) {
        return Err(format!("job {} is running — wait for it to finish", name));
    }
    let mut drop_index_too = drop;
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("not running interactively — pass --yes to confirm".to_string());
        }
        if !confirm(&format!("Remove job {} ({} {})?", name, job.mode(), job.path))? {
            return Err("cancelled".to_string());
        }
        if !drop {
            drop_index_too = confirm(&format!("Also remove its index of {} from {}?", job.path, job.db))?;
        }
    }

    let db = |e: rusqlite::Error| e.to_string();
    conn.execute(
        "DELETE FROM job_run_errors WHERE run IN (SELECT id FROM job_runs WHERE job = ?1)",
        [name],
    )
    .map_err(db)?;
    conn.execute("DELETE FROM job_runs WHERE job = ?1", [name]).map_err(db)?;
    conn.execute("DELETE FROM jobs WHERE name = ?1", [name]).map_err(db)?;
    let _ = fs::remove_file(lock_path(store, name));
    println!("removed job {} and its run history", name);

    if drop_index_too {
        let mut runlog = RunLog::new(&job.db, args.log.as_deref(), args.no_log);
        let result = drop_index(&job.db, &job.path, &mut runlog);
        report_log_failure(&runlog);
        match result {
            Ok(summary) => println!("{}", summary),
            Err(e) => return Err(format!("job removed, but its index wasn't: {}", e)),
        }
    }
    Ok(())
}

fn list(store: &Path) -> Result<(), String> {
    let Some(conn) = open_store(store, false)? else {
        println!("no jobs yet — add one with `diskunder jobs add NAME index|update PATH`");
        return Ok(());
    };
    let jobs = load_jobs(&conn)?;
    if jobs.is_empty() {
        println!("no jobs yet — add one with `diskunder jobs add NAME index|update PATH`");
        return Ok(());
    }
    let mut rows = Vec::new();
    for j in &jobs {
        let last = recent_runs(&conn, &j.name, 1)?.into_iter().next();
        let running = is_running(store, &j.name);
        rows.push(match last {
            None => vec![j.name.clone(), j.mode().into(), j.path.clone(), "never".into(), "—".into(), "—".into(), "—".into(), "—".into()],
            Some(r) => vec![
                j.name.clone(),
                j.mode().into(),
                j.path.clone(),
                fmt_local(&r.started),
                status(&r, running),
                r.duration.map_or("—".to_string(), fmt_secs),
                opt_count(r.found),
                opt_count(r.errors),
            ],
        });
    }
    let mut out = Out::new();
    write_text_table(
        &mut out,
        &["NAME", "MODE", "PATH", "LAST RUN", "STATUS", "DURATION", "FILES", "ERRORS"],
        &[Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Right, Align::Right, Align::Right],
        &rows,
    );
    out.flush();
    Ok(())
}

fn show(store: &Path, name: &str) -> Result<(), String> {
    let conn = open_store(store, false)?.ok_or_else(|| no_such_job(name))?;
    let job = load_job(&conn, name)?.ok_or_else(|| no_such_job(name))?;
    let (created, updated): (String, String) = conn
        .query_row("SELECT created, updated FROM jobs WHERE name = ?1", [name], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?;
    println!("{}", job.name);
    println!("  mode       {}", job.mode());
    println!("  path       {}", job.path);
    println!("  database   {}{}", job.db, if Path::new(&job.db).exists() { "" } else { "  (not created yet)" });
    println!("  threads    {}", job.threads);
    println!("  filters    {}", job.filters_line());
    println!("  created    {} · updated {}", fmt_local(&created), fmt_local(&updated));

    let runs = recent_runs(&conn, name, 10)?;
    if runs.is_empty() {
        println!("\nnever run — `diskunder run {}`", name);
        return Ok(());
    }
    let running = is_running(store, name);
    println!("\nRecent runs");
    let rows: Vec<Vec<String>> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            vec![
                fmt_local(&r.started),
                status(r, running && i == 0),
                r.duration.map_or("—".to_string(), fmt_secs),
                opt_count(r.found),
                opt_count(r.written),
                opt_count(r.removed),
                opt_count(r.errors),
                r.run_id.clone(),
            ]
        })
        .collect();
    let mut out = Out::new();
    write_text_table(
        &mut out,
        &["STARTED", "STATUS", "DURATION", "FOUND", "WRITTEN", "REMOVED", "ERRORS", "RUN"],
        &[Align::Left, Align::Left, Align::Right, Align::Right, Align::Right, Align::Right, Align::Right, Align::Left],
        &rows,
    );
    out.flush();

    let last = &runs[0];
    if let Some(summary) = &last.summary {
        println!("\nLast run: {}", summary);
    }
    if let Some(finished) = &last.finished {
        println!("  finished {}", fmt_local(finished));
    }
    let mut stmt = conn
        .prepare("SELECT message FROM job_run_errors WHERE run = ?1 ORDER BY seq")
        .map_err(|e| e.to_string())?;
    let errors: Vec<String> = stmt
        .query_map([last.id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())?;
    let dropped: i64 = conn
        .query_row("SELECT COALESCE(errors_dropped, 0) FROM job_runs WHERE id = ?1", [last.id], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if !errors.is_empty() {
        println!("\nErrors in the last run");
        for e in &errors {
            println!("  {}", e);
        }
        if dropped > 0 {
            println!("  … and {} more (not kept)", fmt_count(dropped));
        }
    }
    Ok(())
}

/// Logging options that apply to this run only
pub(crate) struct LogArgs {
    pub(crate) log: Option<String>,
    pub(crate) no_log: bool,
    pub(crate) no_tui: bool,
}

/// `run NAME`: run a saved job, recording it in the job's history. Returns the exit code.
fn run(store: &Path, name: Option<&str>, dry_run: bool, threads: Option<usize>, args: &LogArgs) -> Result<i32, String> {
    let Some(name) = name else {
        list(store)?;
        return Err("give a job name: diskunder run NAME".to_string());
    };
    let conn = open_store(store, false)?.ok_or_else(|| no_such_job(name))?;
    let job = load_job(&conn, name)?.ok_or_else(|| no_such_job(name))?;
    // The job's filters always apply, so its runs are the same whoever starts them
    let overrides = Overrides { types: Some(job.filter.types.clone()), paths: Some(job.filter.paths.clone()) };

    if dry_run {
        let root = fs::canonicalize(&job.path).map_err(|e| format!("path not found: {} ({})", job.path, e))?;
        return Ok(run_dry_run(&job.db, &job.path, &root.to_string_lossy(), job.upsert, &overrides));
    }

    // One run per job at a time — the lock is released when the file is closed
    let lock_file = lock_path(store, name);
    fs::create_dir_all(lock_file.parent().unwrap()).map_err(|e| e.to_string())?;
    let lock = File::create(&lock_file).map_err(|e| format!("{}: {}", lock_file.display(), e))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(format!("job {} is already running", name)),
        Err(TryLockError::Error(e)) => return Err(format!("can't lock {}: {}", lock_file.display(), e)),
    }

    let mut runlog = RunLog::new(&job.db, args.log.as_deref(), args.no_log);
    let db = |e: rusqlite::Error| e.to_string();
    // Recorded before starting, so a run that's killed shows as "did not finish"
    conn.execute(
        "INSERT INTO job_runs (job, run_id, started, outcome) VALUES (?1, ?2, datetime('now'), 'running')",
        params![name, runlog.id()],
    )
    .map_err(db)?;
    let row = conn.last_insert_rowid();

    let result = run_indexing(
        IndexRun {
            upsert: job.upsert,
            typed: job.path.clone(),
            db: job.db.clone(),
            threads: threads.unwrap_or(job.threads),
            overrides,
            no_tui: args.no_tui,
        },
        &mut runlog,
    );
    report_log_failure(&runlog);

    let c = &result.counts;
    let (errors, dropped) = runlog.errors();
    conn.execute(
        "UPDATE job_runs SET finished = datetime('now'), outcome = ?2, exit_code = ?3, duration_secs = ?4,
             found = ?5, skipped = ?6, written = ?7, removed = ?8,
             errors_unreadable = ?9, errors_unwritten = ?10, errors_other = ?11,
             excluded_files = ?12, excluded_folders = ?13, rows_excluded_removed = ?14,
             dup_groups = ?15, wasted = ?16, summary = ?17, errors_dropped = ?18
         WHERE id = ?1",
        params![
            row, result.outcome, result.code, runlog.duration_secs() as i64,
            c.found as i64, c.skipped as i64, c.written as i64, c.removed as i64,
            c.errors_unreadable as i64, c.errors_unwritten as i64, c.errors_other as i64,
            c.excluded_files as i64, c.excluded_folders as i64, c.rows_excluded_removed as i64,
            c.dup_groups as i64, c.wasted as i64, result.summary, dropped as i64
        ],
    )
    .map_err(db)?;
    // A run that failed before the run log saw it (e.g. path not found) still records why
    let messages: Vec<String> = if errors.is_empty() && result.outcome == "failed" {
        vec![result.summary.clone()]
    } else {
        errors.to_vec()
    };
    for (seq, message) in messages.iter().enumerate() {
        conn.execute(
            "INSERT INTO job_run_errors (run, seq, message) VALUES (?1, ?2, ?3)",
            params![row, seq as i64, message],
        )
        .map_err(db)?;
    }
    drop(lock);
    Ok(result.code)
}

pub(crate) enum JobsCommand<'a> {
    Add { name: &'a str, settings: JobSettings<'a> },
    Edit { name: &'a str, settings: JobSettings<'a> },
    List,
    Show { name: &'a str },
    Remove { name: &'a str, yes: bool, drop_index: bool },
    Run { name: Option<&'a str>, dry_run: bool, threads: Option<usize> },
}

/// Runs a `jobs` subcommand or `run`; returns the exit code
pub(crate) fn run_jobs_command(store: &Path, cmd: JobsCommand, args: &LogArgs) -> i32 {
    let result = match cmd {
        JobsCommand::Add { name, settings } => add(store, name, &settings).map(|_| 0),
        JobsCommand::Edit { name, settings } => edit(store, name, &settings).map(|_| 0),
        JobsCommand::List => list(store).map(|_| 0),
        JobsCommand::Show { name } => show(store, name).map(|_| 0),
        JobsCommand::Remove { name, yes, drop_index } => remove(store, name, yes, drop_index, args).map(|_| 0),
        JobsCommand::Run { name, dry_run, threads } => run(store, name, dry_run, threads, args),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}
