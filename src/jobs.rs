//! Jobs: named, saved `index` / `update` runs with their run history, kept in a per-user
//! store (not an index database, since each job names its own). The data functions here are
//! shared by the `jobs` / `run` commands and the interactive app.

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
use crate::run::{report_log_failure, run_indexing, IndexRun, RunOutcome};
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

fn db_err(e: rusqlite::Error) -> String {
    e.to_string()
}

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
    if !create && !table_exists(&conn, "jobs").map_err(db_err)? {
        return Ok(None);
    }
    conn.execute_batch(STORE_SCHEMA).map_err(db_err)?;
    Ok(Some(conn))
}

fn open_job(store: &Path, name: &str) -> Result<(Connection, Job), String> {
    let conn = open_store(store, false)?.ok_or_else(|| no_such_job(name))?;
    let job = load_job(&conn, name)?.ok_or_else(|| no_such_job(name))?;
    Ok((conn, job))
}

fn lock_path(store: &Path, name: &str) -> PathBuf {
    store.with_file_name("locks").join(format!("{}.lock", name))
}

/// Whether another process holds the job's lock right now
fn is_running(store: &Path, name: &str) -> bool {
    File::open(lock_path(store, name)).is_ok_and(|f| matches!(f.try_lock(), Err(TryLockError::WouldBlock)))
}

fn no_such_job(name: &str) -> String {
    format!("no job named {} — see `diskunder jobs list`", name)
}

// ─── Jobs ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) upsert: bool,
    pub(crate) path: String,
    pub(crate) db: String,
    pub(crate) threads: usize,
    pub(crate) filter: IndexFilter,
}

impl Job {
    pub(crate) fn mode(&self) -> &'static str {
        if self.upsert { "update" } else { "index" }
    }

    pub(crate) fn filters_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.filter.types.is_empty() {
            parts.push(self.filter.types.describe());
        }
        if !self.filter.paths.is_empty() {
            parts.push(self.filter.paths.describe());
        }
        if parts.is_empty() { "none".to_string() } else { parts.join(" · ") }
    }

    /// The filters the job always runs with
    pub(crate) fn overrides(&self) -> Overrides {
        Overrides { types: Some(self.filter.types.clone()), paths: Some(self.filter.paths.clone()) }
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
        .map_err(db_err)
}

fn load_jobs(conn: &Connection) -> Result<Vec<Job>, String> {
    let mut stmt = conn.prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY name")).map_err(db_err)?;
    let jobs = stmt.query_map([], job_from_row).map_err(db_err)?.collect::<rusqlite::Result<_>>();
    jobs.map_err(db_err)
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
    .map_err(db_err)
}

/// Letters, digits, - and _, so names are safe in cron lines and file names
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(()) } else { Err(format!("job names use letters, digits, - and _ only (up to 64): '{}'", name)) }
}

/// Paths are stored absolute, so cron's working directory doesn't matter. The share may be
/// offline when a job is set up, so a missing path is allowed — with a warning.
fn absolute_path(p: &str, warnings: &mut Vec<String>) -> String {
    match fs::canonicalize(p) {
        Ok(c) => c.to_string_lossy().into_owned(),
        Err(_) => {
            let abs = absolute(p);
            warnings.push(format!("{} doesn't exist right now — saved anyway", abs));
            abs
        }
    }
}

/// The database path as given (or the default), made absolute without requiring it to exist
fn absolute_db(db: &str) -> String {
    std::path::absolute(db).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| db.to_string())
}

/// Settings for creating or editing a job; None keeps the current value (or the default)
pub(crate) struct JobSettings<'a> {
    pub(crate) upsert: Option<bool>,
    pub(crate) path: Option<&'a str>,
    pub(crate) db: Option<&'a str>,
    pub(crate) threads: Option<usize>,
    pub(crate) overrides: Overrides,
}

/// Save a new job. Returns it, plus any warnings (e.g. its path doesn't exist yet).
pub(crate) fn create_job(store: &Path, name: &str, s: &JobSettings) -> Result<(Job, Vec<String>), String> {
    check_name(name)?;
    let path = s.path.ok_or("a job needs a path")?;
    let conn = open_store(store, true)?.expect("created");
    if load_job(&conn, name)?.is_some() {
        return Err(format!("job {} already exists — use `diskunder jobs edit {}`", name, name));
    }
    let mut warnings = Vec::new();
    let job = Job {
        name: name.to_string(),
        upsert: s.upsert.unwrap_or(false),
        path: absolute_path(path, &mut warnings),
        db: absolute_db(s.db.unwrap_or("fileindex.db")),
        threads: s.threads.unwrap_or(4),
        filter: s.overrides.apply(&IndexFilter::default()),
    };
    save_job(&conn, &job, true)?;
    Ok((job, warnings))
}

/// A changed setting: (field, old, new)
pub(crate) type Change = (&'static str, String, String);

/// Change only the settings given. Returns what changed (empty: nothing) and any warnings.
pub(crate) fn update_job(store: &Path, name: &str, s: &JobSettings) -> Result<(Vec<Change>, Vec<String>), String> {
    let (conn, mut job) = open_job(store, name)?;
    let mut warnings = Vec::new();
    let before = (job.mode(), job.path.clone(), job.db.clone(), job.threads, job.filters_line());
    if let Some(u) = s.upsert {
        job.upsert = u;
    }
    if let Some(p) = s.path {
        job.path = absolute_path(p, &mut warnings);
    }
    if let Some(d) = s.db {
        job.db = absolute_db(d);
    }
    if let Some(t) = s.threads {
        job.threads = t;
    }
    job.filter = s.overrides.apply(&job.filter);
    let after = (job.mode(), job.path.clone(), job.db.clone(), job.threads, job.filters_line());
    let changes: Vec<Change> = [
        ("mode", before.0.to_string(), after.0.to_string()),
        ("path", before.1, after.1),
        ("database", before.2, after.2),
        ("threads", before.3.to_string(), after.3.to_string()),
        ("filters", before.4, after.4),
    ]
    .into_iter()
    .filter(|(_, old, new)| old != new)
    .collect();
    if !changes.is_empty() {
        save_job(&conn, &job, false)?;
    }
    Ok((changes, warnings))
}

/// Remove a job and its run history; returns the job (for dropping its index)
pub(crate) fn delete_job(store: &Path, name: &str) -> Result<Job, String> {
    let (conn, job) = open_job(store, name)?;
    if is_running(store, name) {
        return Err(format!("job {} is running — wait for it to finish", name));
    }
    conn.execute("DELETE FROM job_run_errors WHERE run IN (SELECT id FROM job_runs WHERE job = ?1)", [name])
        .map_err(db_err)?;
    conn.execute("DELETE FROM job_runs WHERE job = ?1", [name]).map_err(db_err)?;
    conn.execute("DELETE FROM jobs WHERE name = ?1", [name]).map_err(db_err)?;
    let _ = fs::remove_file(lock_path(store, name));
    Ok(job)
}

// ─── Run history ─────────────────────────────────────────────────────────────

pub(crate) struct RunRow {
    pub(crate) id: i64,
    pub(crate) run_id: String,
    pub(crate) started: String,
    pub(crate) finished: Option<String>,
    pub(crate) outcome: String,
    pub(crate) exit_code: Option<i64>,
    pub(crate) duration: Option<i64>,
    pub(crate) found: Option<i64>,
    pub(crate) skipped: Option<i64>,
    pub(crate) written: Option<i64>,
    pub(crate) removed: Option<i64>,
    pub(crate) errors_unreadable: Option<i64>,
    pub(crate) errors_unwritten: Option<i64>,
    pub(crate) errors_other: Option<i64>,
    pub(crate) excluded_files: Option<i64>,
    pub(crate) excluded_folders: Option<i64>,
    pub(crate) rows_excluded_removed: Option<i64>,
    pub(crate) dup_groups: Option<i64>,
    pub(crate) wasted: Option<i64>,
    pub(crate) summary: Option<String>,
    pub(crate) errors_dropped: Option<i64>,
}

impl RunRow {
    pub(crate) fn errors(&self) -> Option<i64> {
        Some(self.errors_unreadable? + self.errors_unwritten? + self.errors_other?)
    }
}

const RUN_COLUMNS: &str = "id, run_id, started, finished, outcome, exit_code, duration_secs, found, skipped, written,
     removed, errors_unreadable, errors_unwritten, errors_other, excluded_files, excluded_folders,
     rows_excluded_removed, dup_groups, wasted, summary, errors_dropped";

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
        skipped: r.get(8)?,
        written: r.get(9)?,
        removed: r.get(10)?,
        errors_unreadable: r.get(11)?,
        errors_unwritten: r.get(12)?,
        errors_other: r.get(13)?,
        excluded_files: r.get(14)?,
        excluded_folders: r.get(15)?,
        rows_excluded_removed: r.get(16)?,
        dup_groups: r.get(17)?,
        wasted: r.get(18)?,
        summary: r.get(19)?,
        errors_dropped: r.get(20)?,
    })
}

fn recent_runs(conn: &Connection, job: &str, n: Option<usize>) -> Result<Vec<RunRow>, String> {
    let limit = n.map_or(String::new(), |n| format!(" LIMIT {}", n));
    let mut stmt = conn
        .prepare(&format!("SELECT {RUN_COLUMNS} FROM job_runs WHERE job = ?1 ORDER BY id DESC{limit}"))
        .map_err(db_err)?;
    let rows = stmt.query_map([job], run_from_row).map_err(db_err)?.collect::<rusqlite::Result<_>>();
    rows.map_err(db_err)
}

/// "completed", "failed (exit 1)", "running", or "did not finish" for a run that was killed
pub(crate) fn status(r: &RunRow, running_now: bool) -> String {
    match r.outcome.as_str() {
        "running" if running_now => "running".to_string(),
        "running" => "did not finish".to_string(),
        "failed" => format!("failed (exit {})", r.exit_code.unwrap_or(1)),
        other => other.to_string(),
    }
}

/// 14m33s, 2h05m, 9s
pub(crate) fn fmt_secs(s: i64) -> String {
    match s {
        0..=59 => format!("{}s", s),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub(crate) fn opt_count(n: Option<i64>) -> String {
    n.map_or("—".to_string(), fmt_count)
}

/// A job with its latest run, and whether a run holds its lock right now
pub(crate) struct JobSummary {
    pub(crate) job: Job,
    pub(crate) last: Option<RunRow>,
    pub(crate) running: bool,
}

/// Every job, by name, with its latest run. Empty if there's no store yet.
pub(crate) fn job_summaries(store: &Path) -> Result<Vec<JobSummary>, String> {
    let Some(conn) = open_store(store, false)? else { return Ok(Vec::new()) };
    load_jobs(&conn)?
        .into_iter()
        .map(|job| {
            let last = recent_runs(&conn, &job.name, Some(1))?.into_iter().next();
            let running = is_running(store, &job.name);
            Ok(JobSummary { job, last, running })
        })
        .collect()
}

/// A job's runs, newest first
pub(crate) fn job_history(store: &Path, name: &str, n: Option<usize>) -> Result<Vec<RunRow>, String> {
    let (conn, _) = open_job(store, name)?;
    recent_runs(&conn, name, n)
}

/// The error messages kept for a run (up to 50)
pub(crate) fn run_errors(store: &Path, run: i64) -> Result<Vec<String>, String> {
    let Some(conn) = open_store(store, false)? else { return Ok(Vec::new()) };
    let mut stmt = conn.prepare("SELECT message FROM job_run_errors WHERE run = ?1 ORDER BY seq").map_err(db_err)?;
    let errors = stmt.query_map([run], |r| r.get(0)).map_err(db_err)?.collect::<rusqlite::Result<_>>();
    errors.map_err(db_err)
}

/// Every run id a job has recorded — to pick its lines out of a run log
pub(crate) fn job_run_ids(store: &Path, name: &str) -> Result<Vec<String>, String> {
    Ok(job_history(store, name, None)?.into_iter().map(|r| r.run_id).collect())
}

// ─── Running a job ───────────────────────────────────────────────────────────

/// A job run that has started: it holds the job's lock and has a history row
pub(crate) struct ActiveRun {
    conn: Connection,
    row: i64,
    _lock: File, // released when dropped
}

/// Lock the job and record that a run has started (so a run that's killed shows as
/// "did not finish"). Returns the run to perform; `threads` overrides the job's for this run.
pub(crate) fn begin_run(
    store: &Path,
    name: &str,
    threads: Option<usize>,
    no_tui: bool,
    runlog: &RunLog,
) -> Result<(ActiveRun, IndexRun), String> {
    let (conn, job) = open_job(store, name)?;
    // One run per job at a time — the lock is released when the file is closed
    let lock_file = lock_path(store, name);
    fs::create_dir_all(lock_file.parent().unwrap()).map_err(|e| e.to_string())?;
    let lock = File::create(&lock_file).map_err(|e| format!("{}: {}", lock_file.display(), e))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(format!("job {} is already running", name)),
        Err(TryLockError::Error(e)) => return Err(format!("can't lock {}: {}", lock_file.display(), e)),
    }
    conn.execute(
        "INSERT INTO job_runs (job, run_id, started, outcome) VALUES (?1, ?2, datetime('now'), 'running')",
        params![name, runlog.id()],
    )
    .map_err(db_err)?;
    let row = conn.last_insert_rowid();
    let run = IndexRun {
        upsert: job.upsert,
        typed: job.path.clone(),
        db: job.db.clone(),
        threads: threads.unwrap_or(job.threads),
        overrides: job.overrides(),
        no_tui,
    };
    Ok((ActiveRun { conn, row, _lock: lock }, run))
}

impl ActiveRun {
    /// Complete the history row with the outcome, counts and errors; releases the lock
    pub(crate) fn finish(self, result: &RunOutcome, runlog: &RunLog) -> Result<(), String> {
        let c = &result.counts;
        let (errors, dropped) = runlog.errors();
        self.conn
            .execute(
                "UPDATE job_runs SET finished = datetime('now'), outcome = ?2, exit_code = ?3, duration_secs = ?4,
                     found = ?5, skipped = ?6, written = ?7, removed = ?8,
                     errors_unreadable = ?9, errors_unwritten = ?10, errors_other = ?11,
                     excluded_files = ?12, excluded_folders = ?13, rows_excluded_removed = ?14,
                     dup_groups = ?15, wasted = ?16, summary = ?17, errors_dropped = ?18
                 WHERE id = ?1",
                params![
                    self.row, result.outcome, result.code, runlog.duration_secs() as i64,
                    c.found as i64, c.skipped as i64, c.written as i64, c.removed as i64,
                    c.errors_unreadable as i64, c.errors_unwritten as i64, c.errors_other as i64,
                    c.excluded_files as i64, c.excluded_folders as i64, c.rows_excluded_removed as i64,
                    c.dup_groups as i64, c.wasted as i64, result.summary, dropped as i64
                ],
            )
            .map_err(db_err)?;
        // A run that failed before the run log saw it (e.g. path not found) still records why
        let messages: Vec<String> =
            if errors.is_empty() && result.outcome == "failed" { vec![result.summary.clone()] } else { errors.to_vec() };
        for (seq, message) in messages.iter().enumerate() {
            self.conn
                .execute(
                    "INSERT INTO job_run_errors (run, seq, message) VALUES (?1, ?2, ?3)",
                    params![self.row, seq as i64, message],
                )
                .map_err(db_err)?;
        }
        Ok(())
    }
}

// ─── Commands ────────────────────────────────────────────────────────────────

fn add(store: &Path, name: &str, s: &JobSettings) -> Result<(), String> {
    let (job, warnings) = create_job(store, name, s)?;
    for w in warnings {
        eprintln!("warning: {}", w);
    }
    println!("added job {}: {} {} → {}", job.name, job.mode(), job.path, job.db);
    println!("  threads {} · filters {}", job.threads, job.filters_line());
    println!("run it with: diskunder run {}", job.name);
    Ok(())
}

fn edit(store: &Path, name: &str, s: &JobSettings) -> Result<(), String> {
    let (changes, warnings) = update_job(store, name, s)?;
    for w in warnings {
        eprintln!("warning: {}", w);
    }
    if changes.is_empty() {
        println!("job {} unchanged", name);
        return Ok(());
    }
    println!("updated job {}:", name);
    for (field, old, new) in changes {
        println!("  {:<9} {} → {}", field, old, new);
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool, String> {
    eprint!("{} [y/N] ", question);
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn remove(store: &Path, name: &str, yes: bool, drop: bool, args: &LogArgs) -> Result<(), String> {
    let (_, job) = open_job(store, name)?;
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
    let job = delete_job(store, name)?;
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
    let jobs = job_summaries(store)?;
    if jobs.is_empty() {
        println!("no jobs yet — add one with `diskunder jobs add NAME index|update PATH`");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = jobs
        .iter()
        .map(|s| {
            let j = &s.job;
            match &s.last {
                None => vec![j.name.clone(), j.mode().into(), j.path.clone(), "never".into(), "—".into(), "—".into(), "—".into(), "—".into()],
                Some(r) => vec![
                    j.name.clone(),
                    j.mode().into(),
                    j.path.clone(),
                    fmt_local(&r.started),
                    status(r, s.running),
                    r.duration.map_or("—".to_string(), fmt_secs),
                    opt_count(r.found),
                    opt_count(r.errors()),
                ],
            }
        })
        .collect();
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
    let (conn, job) = open_job(store, name)?;
    let (created, updated): (String, String) = conn
        .query_row("SELECT created, updated FROM jobs WHERE name = ?1", [name], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db_err)?;
    println!("{}", job.name);
    println!("  mode       {}", job.mode());
    println!("  path       {}", job.path);
    println!("  database   {}{}", job.db, if Path::new(&job.db).exists() { "" } else { "  (not created yet)" });
    println!("  threads    {}", job.threads);
    println!("  filters    {}", job.filters_line());
    println!("  created    {} · updated {}", fmt_local(&created), fmt_local(&updated));

    let runs = recent_runs(&conn, name, Some(10))?;
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
                opt_count(r.errors()),
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
    let errors = run_errors(store, last.id)?;
    if !errors.is_empty() {
        println!("\nErrors in the last run");
        for e in &errors {
            println!("  {}", e);
        }
        let dropped = last.errors_dropped.unwrap_or(0);
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
    let (_, job) = open_job(store, name)?;
    if dry_run {
        let root = fs::canonicalize(&job.path).map_err(|e| format!("path not found: {} ({})", job.path, e))?;
        return Ok(run_dry_run(&job.db, &job.path, &root.to_string_lossy(), job.upsert, &job.overrides()));
    }

    let mut runlog = RunLog::new(&job.db, args.log.as_deref(), args.no_log);
    let (active, index_run) = begin_run(store, name, threads, args.no_tui, &runlog)?;
    let result = run_indexing(index_run, &mut runlog);
    report_log_failure(&runlog);
    active.finish(&result, &runlog)?;
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
