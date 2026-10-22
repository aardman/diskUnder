// diskUnder v4 — TUI progress, live duplicate results and database maintenance
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
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, IsTerminal, Read, Stdout},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, SystemTime},
};
use clap::{Parser, Subcommand};
use walkdir::WalkDir;

const BATCH_SIZE: usize = 1_000;
const READ_CHUNK: usize = 65_536; // 64 KB
const TICK: Duration = Duration::from_millis(100); // UI refresh rate

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "diskunder", about = "File indexer with live TUI progress", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,

    /// SQLite database file (created if absent)
    #[arg(long, default_value = "fileindex.db", global = true)]
    db: String,

    /// Parallel hashing threads — keep low for network shares (e.g. --threads 4)
    #[arg(long, default_value_t = 4, global = true)]
    threads: usize,

    /// Disable the TUI and print plain progress lines instead.
    /// Enabled automatically when stdout is not a terminal (cron, SSH, pipes).
    #[arg(long, global = true)]
    no_tui: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Index every file under PATH; files already in the database are skipped,
    /// so an interrupted run can simply be restarted
    Index {
        /// Directory to index
        path: String,
    },
    /// Re-index new and changed files under PATH, and drop rows for deleted files
    Update {
        /// Directory to update
        path: String,
    },
    /// Inspect and maintain the index database
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
}

#[derive(Subcommand)]
enum DbAction {
    /// List indexed paths with file counts, duplicates, size and last run
    Info,
    /// Remove the index for a path (asks for confirmation)
    Drop {
        /// Indexed path to remove, as listed by `db info`
        #[arg(required_unless_present = "table")]
        path: Option<String>,
        /// Remove a table by name instead, e.g. one `db info` lists with no recorded path
        #[arg(long, conflicts_with = "path")]
        table: Option<String>,
        /// Don't ask for confirmation (required when not run from a terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Compact the database file after rows or tables have been removed
    Vacuum,
    /// Run SQLite's integrity check and report tables with no recorded path
    Check,
}

/// "0.4.1" → "4.1": the project numbers releases v1, v2, … while the crate stays at 0.x
fn display_version() -> &'static str {
    env!("CARGO_PKG_VERSION").trim_start_matches("0.")
}

/// (device, inode) — identifies the underlying file, so hard links to the same data
/// are recognised as one copy. None where the platform or filesystem doesn't provide it.
type FileId = (u64, u64);

#[cfg(unix)]
fn file_id(m: &fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_: &fs::Metadata) -> Option<FileId> {
    None
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
        file_id: Option<FileId>,
    },
    /// A queued file that could not be read or hashed
    FileError { path: String, error: String },
    /// A traversal entry or database operation failed (not tied to a queued file)
    Error { context: String, error: String },
    /// Informational line for the activity log (also printed in plain mode)
    Info(String),
    /// A batch of N rows committed to SQLite
    BatchWritten(usize),
    /// N rows removed for files no longer on disk (update mode)
    Removed(usize),
    /// A batch of `files` hashed files could not be committed
    BatchFailed { files: usize, error: String },
    /// Checksums of files skipped this run (already indexed / unchanged), sent once after
    /// traversal so duplicate detection covers the whole table, not just files hashed now
    Seed(Vec<(String, u64, String, Option<FileId>)>), // (path, size, checksum, file_id)
    /// Directory traversal finished — `to_hash` files queued, `skipped` already in the DB
    /// (index mode) or unchanged since last indexed (update mode)
    TraversalComplete { to_hash: usize, skipped: usize },
    /// All work done
    Done,
}

// ─── Duplicate tracking ──────────────────────────────────────────────────────

struct DupGroup {
    size: u64,
    paths: Vec<String>,
    copies: usize, // distinct files; paths.len() − copies are extra hard links
    wasted: u64,   // size × (copies − 1)
}

/// Every path seen with one checksum. Paths sharing a FileId are hard links to the same
/// data, so they count as one copy; paths with no FileId each count as their own copy.
struct ChecksumGroup {
    size: u64,
    paths: Vec<String>,
    ids: HashSet<FileId>,
    unknown_ids: usize,
}

impl ChecksumGroup {
    fn copies(&self) -> usize {
        self.ids.len() + self.unknown_ids
    }
}

// ─── Application state ───────────────────────────────────────────────────────

struct AppState {
    mode: String,
    target_path: String,
    db_path: String,

    files_found: usize,
    files_skipped: usize, // already indexed (index mode) or unchanged (update mode)
    files_hashed: usize,
    files_written: usize,
    files_removed: usize, // rows dropped for deleted files (update mode)
    files_errored: usize,  // queued files that could not be read or hashed
    files_unwritten: usize, // hashed files lost to a failed batch commit
    other_errors: usize,   // traversal and database errors
    total_files: Option<usize>, // set once traversal is complete

    current_file: String,

    // In-memory checksum index for live duplicate detection.
    // For very large runs (millions of files) this will grow proportionally.
    checksum_map: HashMap<String, ChecksumGroup>,
    dup_keys: HashSet<String>,                         // checksums with 2+ distinct copies
    dups_dirty: bool,                                  // top_dups needs refreshing
    top_dups: Vec<DupGroup>,                           // top 10 by wasted bytes
    dup_wasted: u64,                                   // wasted bytes across all groups

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
            files_skipped: 0,
            files_hashed: 0,
            files_written: 0,
            files_removed: 0,
            files_errored: 0,
            files_unwritten: 0,
            other_errors: 0,
            total_files: None,
            current_file: String::new(),
            checksum_map: HashMap::new(),
            dup_keys: HashSet::new(),
            dups_dirty: false,
            top_dups: Vec::new(),
            dup_wasted: 0,
            log: Vec::new(),
            done: false,
        }
    }

    fn skipped_label(&self) -> &'static str {
        if self.mode == "update" { "unchanged" } else { "already indexed" }
    }

    fn errors_total(&self) -> usize {
        self.files_errored + self.files_unwritten + self.other_errors
    }

    /// e.g. "3 errors (2 unreadable, 1 not written)"; just "0 errors" when clean
    fn errors_summary(&self) -> String {
        let parts: Vec<String> = [
            (self.files_errored, "unreadable"),
            (self.files_unwritten, "not written"),
            (self.other_errors, "traversal/db"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{} {}", n, label))
        .collect();
        if parts.is_empty() {
            "0 errors".to_string()
        } else {
            format!("{} errors ({})", self.errors_total(), parts.join(", "))
        }
    }

    fn progress(&self) -> f64 {
        match self.total_files {
            // Count unreadable files as processed — otherwise permission-denied files on
            // /Users/ or a network share leave the bar stuck below 100% when the run is done.
            // Traversal errors are not counted: those entries were never queued in `n`.
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

    fn record_hashed(&mut self, path: String, size: u64, checksum: String, id: Option<FileId>) {
        self.files_hashed += 1;
        self.current_file = path.clone();
        self.track_checksum(path, size, checksum, id);
    }

    fn track_checksum(&mut self, path: String, size: u64, checksum: String, id: Option<FileId>) {
        // Empty files all share one checksum and waste nothing — not worth tracking
        if size == 0 {
            return;
        }

        let add = |g: &mut ChecksumGroup, path: String| {
            g.paths.push(path);
            match id {
                Some(id) => {
                    g.ids.insert(id);
                }
                None => g.unknown_ids += 1,
            }
        };
        match self.checksum_map.get_mut(&checksum) {
            Some(group) => {
                add(group, path);
                if group.copies() >= 2 {
                    self.dups_dirty = true;
                    if !self.dup_keys.contains(&checksum) {
                        self.dup_keys.insert(checksum);
                    }
                }
            }
            None => {
                let mut group = ChecksumGroup {
                    size,
                    paths: Vec::new(),
                    ids: HashSet::new(),
                    unknown_ids: 0,
                };
                add(&mut group, path);
                self.checksum_map.insert(checksum, group);
            }
        }
    }

    /// Recompute the top-10 duplicate groups if anything changed since the last call.
    /// Called once per UI tick rather than per file, and only scans groups that have
    /// duplicates, so cost doesn't grow with the number of files hashed.
    fn refresh_top_dups(&mut self) {
        if !self.dups_dirty {
            return;
        }
        self.dups_dirty = false;

        let mut ranked: Vec<(u64, &String)> = self
            .dup_keys
            .iter()
            .map(|k| {
                let g = &self.checksum_map[k];
                (g.size * (g.copies() as u64 - 1), k)
            })
            .collect();
        self.dup_wasted = ranked.iter().map(|(w, _)| w).sum();
        let by_wasted = |a: &(u64, &String), b: &(u64, &String)| b.0.cmp(&a.0);
        if ranked.len() > 10 {
            ranked.select_nth_unstable_by(9, by_wasted);
            ranked.truncate(10);
        }
        ranked.sort_unstable_by(by_wasted);

        self.top_dups = ranked
            .into_iter()
            .map(|(wasted, k)| {
                let g = &self.checksum_map[k];
                DupGroup { size: g.size, paths: g.paths.clone(), copies: g.copies(), wasted }
            })
            .collect();
    }

    fn apply(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::FileFound => self.files_found += 1,
            WorkerMsg::FileHashed { path, size, checksum, file_id } => {
                self.record_hashed(path, size, checksum, file_id);
            }
            WorkerMsg::FileError { path, error } => {
                self.files_errored += 1;
                self.push_log(format!("warn: {} — {}", path, error));
            }
            WorkerMsg::Error { context, error } => {
                self.other_errors += 1;
                self.push_log(format!("warn: {} — {}", context, error));
            }
            WorkerMsg::Info(line) => self.push_log(line),
            WorkerMsg::Seed(entries) => {
                for (path, size, checksum, file_id) in entries {
                    self.track_checksum(path, size, checksum, file_id);
                }
            }
            WorkerMsg::BatchWritten(n) => self.files_written += n,
            WorkerMsg::Removed(n) => {
                self.files_removed = n;
                self.push_log(format!("removed {} rows for files no longer on disk", n));
            }
            WorkerMsg::BatchFailed { files, error } => {
                self.files_unwritten += files;
                self.push_log(format!("error: batch of {} files not written — {}", files, error));
            }
            WorkerMsg::TraversalComplete { to_hash, skipped } => {
                self.total_files = Some(to_hash);
                self.files_skipped = skipped;
                if skipped > 0 {
                    self.push_log(format!(
                        "traversal complete — {} files found, {} {}, {} to hash",
                        self.files_found, skipped, self.skipped_label(), to_hash
                    ));
                } else {
                    self.push_log(format!("traversal complete — {} files found", to_hash));
                }
            }
            WorkerMsg::Done => {
                self.done = true;
                self.refresh_top_dups();
                self.push_log(format!(
                    "done — {} written, {}, {} duplicate groups",
                    self.files_written,
                    self.errors_summary(),
                    self.dup_keys.len()
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
    birth_date: String,
    checksum: String,
    extension: String,
    directory: String,
    file_id: Option<FileId>,
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
            birth_date: metadata
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
            file_id: file_id(&metadata),
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

fn table_name_for(root: &str) -> String {
    format!("files_{:x}", md5::compute(root))
}

fn table_exists(conn: &Connection, t: &str) -> DbResult<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [t],
        |row| row.get(0),
    )
}

/// Before v4 the table name came from the path exactly as typed, so `/nas`, `/nas/` and
/// `./nas` got separate tables, with stored paths in that spelling (possibly relative).
/// If a table exists for the typed spelling but not for the canonical root, rewrite its
/// paths onto the canonical root and rename it, so nothing needs re-hashing.
/// Returns the number of rows migrated, or None if there was nothing to do.
fn migrate_legacy_table(conn: &mut Connection, typed: &str, root: &str) -> Result<Option<usize>, String> {
    let old = table_name_for(typed);
    let new = table_name_for(root);
    let db = |e: rusqlite::Error| e.to_string();
    if old == new || !table_exists(conn, &old).map_err(db)? || table_exists(conn, &new).map_err(db)? {
        return Ok(None);
    }

    let tx = conn.transaction().map_err(db)?;
    let rows: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(&format!("SELECT id, path FROM {old}")).map_err(db)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db)?
            .collect::<DbResult<_>>()
            .map_err(db)?;
        rows
    };
    {
        let mut update = tx
            .prepare(&format!("UPDATE {old} SET path = ?1, directory = ?2 WHERE id = ?3"))
            .map_err(db)?;
        for (id, path) in &rows {
            // Component-wise, so `./nas`, `nas//` and `nas/` prefixes all strip cleanly
            let rel = Path::new(path)
                .strip_prefix(typed)
                .map_err(|_| format!("row path {} is not under {}", path, typed))?;
            let new_path = if rel.as_os_str().is_empty() {
                PathBuf::from(root)
            } else {
                Path::new(root).join(rel)
            };
            let directory = new_path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            update
                .execute(params![new_path.to_string_lossy(), directory, id])
                .map_err(db)?;
        }
    }

    // Index names embed the table name; drop the old ones; they're recreated under the new name
    let old_indexes: Vec<String> = {
        let mut stmt = tx
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?1 AND name LIKE 'idx_%'")
            .map_err(db)?;
        let names = stmt
            .query_map([&old], |row| row.get(0))
            .map_err(db)?
            .collect::<DbResult<_>>()
            .map_err(db)?;
        names
    };
    for name in old_indexes {
        tx.execute(&format!("DROP INDEX {name}"), []).map_err(db)?;
    }
    tx.execute(&format!("ALTER TABLE {old} RENAME TO {new}"), []).map_err(db)?;
    tx.commit().map_err(db)?;
    Ok(Some(rows.len()))
}

fn create_table(conn: &Connection, path: &str) -> DbResult<String> {
    let table_name = table_name_for(path);
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {t} (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                path          TEXT NOT NULL UNIQUE,
                filename      TEXT NOT NULL,
                size          INTEGER NOT NULL,
                modified_date TEXT,
                access_date   TEXT,
                birth_date    TEXT,       -- creation time where the filesystem has one
                checksum      TEXT,
                extension     TEXT,
                directory     TEXT,
                created_at    TEXT DEFAULT CURRENT_TIMESTAMP,
                device        INTEGER,    -- with inode, identifies hard links
                inode         INTEGER
            )",
            t = table_name
        ),
        [],
    )?;
    // Before v4 this column was called change_date, but it has always held the birth
    // (creation) time, not the inode change time. Rename in place; no data is rewritten.
    let has_old_column: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{t}') WHERE name = 'change_date')", t = table_name),
        [],
        |row| row.get(0),
    )?;
    if has_old_column {
        conn.execute(
            &format!("ALTER TABLE {t} RENAME COLUMN change_date TO birth_date", t = table_name),
            [],
        )?;
    }
    // Added in v4; existing rows stay NULL until re-hashed or backfilled by `update`
    for col in ["device", "inode"] {
        let exists: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{t}') WHERE name = '{col}')", t = table_name),
            [],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute(&format!("ALTER TABLE {t} ADD COLUMN {col} INTEGER", t = table_name), [])?;
        }
    }
    conn.execute(
        &format!(
            "CREATE INDEX IF NOT EXISTS idx_{t}_path ON {t}(path)",
            t = table_name
        ),
        [],
    )?;
    Ok(table_name)
}

/// One row per indexed path: which table holds it and how its last run went. Table names
/// are an MD5 of the path, so without this there's no way to tell which table is which.
const ROOTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS roots (
    table_name    TEXT PRIMARY KEY,
    root          TEXT NOT NULL,
    first_run     TEXT NOT NULL,   -- UTC, like the file dates
    last_started  TEXT NOT NULL,
    last_finished TEXT,            -- NULL while a run is in progress or if it was interrupted
    last_mode     TEXT NOT NULL    -- index | update
)";

fn roots_run_started(conn: &Connection, table: &str, root: &str, mode: &str) -> DbResult<()> {
    conn.execute(ROOTS_SCHEMA, [])?;
    conn.execute(
        "INSERT INTO roots (table_name, root, first_run, last_started, last_finished, last_mode)
         VALUES (?1, ?2, datetime('now'), datetime('now'), NULL, ?3)
         ON CONFLICT(table_name) DO UPDATE SET
           root = excluded.root, last_started = excluded.last_started,
           last_finished = NULL, last_mode = excluded.last_mode",
        params![table, root, mode],
    )?;
    Ok(())
}

fn roots_run_finished(conn: &Connection, table: &str) -> DbResult<()> {
    conn.execute(
        "UPDATE roots SET last_finished = datetime('now') WHERE table_name = ?1",
        [table],
    )?;
    Ok(())
}

struct Stored {
    size: u64,
    modified: String,
    checksum: String,
    file_id: Option<FileId>,
}

// SQLite integers are signed 64-bit; device and inode numbers are stored bit-for-bit
fn id_to_db(id: Option<FileId>) -> (Option<i64>, Option<i64>) {
    (id.map(|(d, _)| d as i64), id.map(|(_, i)| i as i64))
}

fn id_from_db(device: Option<i64>, inode: Option<i64>) -> Option<FileId> {
    Some((device? as u64, inode? as u64))
}

/// path → stored size, modified date and checksum for every row already in the table
fn load_indexed(conn: &Connection, t: &str) -> DbResult<HashMap<String, Stored>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT path, size, COALESCE(modified_date, ''), COALESCE(checksum, ''), device, inode FROM {t}",
        t = t
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                Stored {
                    size: row.get(1)?,
                    modified: row.get(2)?,
                    checksum: row.get(3)?,
                    file_id: id_from_db(row.get(4)?, row.get(5)?),
                },
            ))
        })?
        .collect();
    rows
}

/// Record device/inode for rows that predate those columns, without re-hashing
fn update_file_ids(conn: &mut Connection, t: &str, rows: &[(String, Option<FileId>)]) -> DbResult<usize> {
    let tx = conn.transaction()?;
    let mut count = 0usize;
    {
        let mut stmt =
            tx.prepare(&format!("UPDATE {t} SET device = ?1, inode = ?2 WHERE path = ?3", t = t))?;
        for (path, id) in rows {
            let (device, inode) = id_to_db(*id);
            count += stmt.execute(params![device, inode, path])?;
        }
    }
    tx.commit()?;
    Ok(count)
}

fn delete_paths(conn: &mut Connection, t: &str, paths: &[String]) -> DbResult<usize> {
    let tx = conn.transaction()?;
    let mut count = 0usize;
    {
        let mut stmt = tx.prepare(&format!("DELETE FROM {t} WHERE path = ?1", t = t))?;
        for p in paths {
            count += stmt.execute([p])?;
        }
    }
    tx.commit()?;
    Ok(count)
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
            "INSERT INTO {t} (path,filename,size,modified_date,access_date,birth_date,checksum,extension,directory,device,inode)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(path) DO UPDATE SET
               size=excluded.size, modified_date=excluded.modified_date,
               access_date=excluded.access_date, birth_date=excluded.birth_date,
               checksum=excluded.checksum, extension=excluded.extension,
               directory=excluded.directory, device=excluded.device, inode=excluded.inode",
            t = t
        )
    } else {
        format!(
            "INSERT OR IGNORE INTO {t}
             (path,filename,size,modified_date,access_date,birth_date,checksum,extension,directory,device,inode)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            t = t
        )
    };

    let tx = conn.transaction()?;
    let mut count = 0usize;
    for e in batch {
        let (device, inode) = id_to_db(e.file_id);
        count += tx.execute(
            &sql,
            params![e.path, e.filename, e.size, e.modified_date, e.access_date,
                    e.birth_date, e.checksum, e.extension, e.directory, device, inode],
        )?;
    }
    tx.commit()?;
    Ok(count)
}

// ─── Worker thread ───────────────────────────────────────────────────────────

/// `path` is the canonical root; `typed` is the root as given on the command line,
/// used only to find a table created by an earlier version.
fn run_worker(
    path: String,
    typed: String,
    db_path: String,
    threads: usize,
    upsert: bool,
    tx: Sender<WorkerMsg>,
) {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let mut conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(WorkerMsg::Error { context: db_path, error: e.to_string() });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    };
    let _ = conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY; PRAGMA cache_size=100000;",
    );

    match migrate_legacy_table(&mut conn, &typed, &path) {
        Ok(Some(n)) => {
            let _ = tx.send(WorkerMsg::Info(format!(
                "migrated {} rows from the table for \"{}\" to {}",
                n, typed, path
            )));
        }
        Ok(None) => {}
        Err(e) => {
            // Carrying on would re-hash everything into a new table — stop instead
            let _ = tx.send(WorkerMsg::Error {
                context: "db".to_string(),
                error: format!("could not migrate table for \"{}\": {}", typed, e),
            });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    }

    let table_name = match create_table(&conn, &path) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(WorkerMsg::Error { context: path, error: e.to_string() });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    };
    let mode = if upsert { "update" } else { "index" };
    if let Err(e) = roots_run_started(&conn, &table_name, &path, mode) {
        let _ = tx.send(WorkerMsg::Error {
            context: "db".to_string(),
            error: format!("could not record run in roots table: {}", e),
        });
    }

    // Files already in the table are not re-hashed: in index mode any existing path is
    // skipped (so an interrupted run resumes cheaply); in update mode a path is skipped
    // only if its size and modified time still match what was stored.
    let mut indexed = load_indexed(&conn, &table_name).unwrap_or_else(|e| {
        let _ = tx.send(WorkerMsg::Error {
            context: "db".to_string(),
            error: format!("could not read existing paths, re-hashing all: {}", e),
        });
        HashMap::new()
    });

    // ── Traversal ──
    let mut files: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;
    let mut seed: Vec<(String, u64, String, Option<FileId>)> = Vec::new();
    let mut backfill: Vec<(String, Option<FileId>)> = Vec::new(); // unchanged rows missing device/inode
    // Paths that could not be traversed; rows beneath them are not treated as deleted.
    // None means an error had no path, so no deletions are safe.
    let mut unreadable: Option<Vec<PathBuf>> = Some(Vec::new());

    for entry in WalkDir::new(&path) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let _ = tx.send(WorkerMsg::FileFound);
                // Seen paths are removed from `indexed`; in update mode whatever remains
                // afterwards has been deleted from disk.
                let stored = indexed.remove_entry(e.path().to_string_lossy().as_ref());
                let keep = match stored {
                    // index mode: any existing row is kept as-is
                    Some(row) if !upsert => Some(row),
                    // update mode: kept only if size and modified time still match
                    Some((p, mut s)) => {
                        let meta = e.metadata().ok();
                        let unchanged = meta.as_ref().map_or(false, |m| {
                            m.len() == s.size
                                && m.modified().ok().map(fmt_time).unwrap_or_default() == s.modified
                        });
                        if unchanged {
                            let current = meta.as_ref().and_then(file_id);
                            if current.is_some() && current != s.file_id {
                                backfill.push((p.clone(), current));
                                s.file_id = current;
                            }
                        }
                        unchanged.then_some((p, s))
                    }
                    None => None,
                };
                match keep {
                    Some((p, s)) => {
                        skipped += 1;
                        if !s.checksum.is_empty() {
                            seed.push((p, s.size, s.checksum, s.file_id));
                        }
                    }
                    None => files.push(e.path().to_owned()),
                }
            }
            Ok(_) => {}
            Err(e) => {
                match (e.path(), unreadable.as_mut()) {
                    (Some(p), Some(list)) => list.push(p.to_owned()),
                    _ => unreadable = None,
                }
                let _ = tx.send(WorkerMsg::Error {
                    context: e.path().map(|p| p.display().to_string()).unwrap_or_default(),
                    error: e.to_string(),
                });
            }
        }
    }

    let found_any = !files.is_empty() || skipped > 0;
    let _ = tx.send(WorkerMsg::TraversalComplete { to_hash: files.len(), skipped });
    let _ = tx.send(WorkerMsg::Seed(seed));

    if !backfill.is_empty() {
        match update_file_ids(&mut conn, &table_name, &backfill) {
            Ok(n) => {
                let _ = tx.send(WorkerMsg::Info(format!(
                    "recorded device/inode for {} existing rows (no re-hash needed)",
                    n
                )));
            }
            Err(e) => {
                let _ = tx.send(WorkerMsg::Error {
                    context: "db".to_string(),
                    error: format!("could not record device/inode: {}", e),
                });
            }
        }
    }

    // ── Drop rows for deleted files (update mode) ──
    if upsert && !indexed.is_empty() {
        match unreadable {
            // An empty walk usually means an unmounted share, not a deleted tree
            _ if !found_any => {
                let _ = tx.send(WorkerMsg::Error {
                    context: path.clone(),
                    error: format!(
                        "no files found — not removing {} existing rows (is the share mounted?)",
                        indexed.len()
                    ),
                });
            }
            None => {
                let _ = tx.send(WorkerMsg::Error {
                    context: "db".to_string(),
                    error: "traversal errors without a path — not removing deleted files this run"
                        .to_string(),
                });
            }
            Some(skip_under) => {
                let gone: Vec<String> = indexed
                    .into_keys()
                    .filter(|p| !skip_under.iter().any(|d| Path::new(p).starts_with(d)))
                    .collect();
                match delete_paths(&mut conn, &table_name, &gone) {
                    Ok(n) => {
                        let _ = tx.send(WorkerMsg::Removed(n));
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::Error {
                            context: "db".to_string(),
                            error: format!("could not remove deleted files: {}", e),
                        });
                    }
                }
            }
        }
    } else {
        drop(indexed);
    }

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
                        file_id: entry.file_id,
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
                    let _ = tx.send(WorkerMsg::BatchFailed {
                        files: batch.len(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }

    // IF NOT EXISTS makes this cheap when they're already there; also covers a table
    // whose first run was `update`
    if let Err(e) = create_indexes(&conn, &table_name) {
        let _ = tx.send(WorkerMsg::Error {
            context: "db".to_string(),
            error: format!("could not create indexes: {}", e),
        });
    }

    if let Err(e) = roots_run_finished(&conn, &table_name) {
        let _ = tx.send(WorkerMsg::Error {
            context: "db".to_string(),
            error: format!("could not record run in roots table: {}", e),
        });
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
            Span::raw("  Hashed  "),
            Span::styled(
                format!("{:>9}", state.files_hashed),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!(" / {}", total_str),
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

// ─── Plain-text fallback (no TTY / --no-tui) ─────────────────────────────────

fn run_plain(state: &mut AppState, rx: Receiver<WorkerMsg>) {
    eprintln!("{} {}", state.mode, state.target_path);

    for msg in rx {
        // Per-file read errors are only counted (there can be thousands); traversal and
        // database errors are rare and need explaining, so print them
        match &msg {
            WorkerMsg::Info(line) => eprintln!("{}", line),
            WorkerMsg::Error { context, error } => eprintln!("warn: {} — {}", context, error),
            _ => {}
        }
        let print_progress = matches!(msg, WorkerMsg::BatchWritten(_) | WorkerMsg::BatchFailed { .. });
        let done = matches!(msg, WorkerMsg::Done);

        state.apply(msg);

        if print_progress {
            let processed = state.files_hashed + state.files_errored;
            let total_str = state
                .total_files
                .map(|n| n.to_string())
                .unwrap_or_else(|| "…".into());
            eprintln!(
                "  {}/{} processed — {} written, {} errors",
                processed, total_str, state.files_written, state.errors_total()
            );
        }

        if done {
            break;
        }
    }

    eprintln!(
        "complete: {} files — {} {}, {} written, {} removed, {}, {} duplicate groups",
        state.files_found,
        state.files_skipped,
        state.skipped_label(),
        state.files_written,
        state.files_removed,
        state.errors_summary(),
        state.dup_keys.len(),
    );
    if !state.dup_keys.is_empty() {
        eprintln!("wasted: {}", fmt_bytes(state.dup_wasted));
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

// ─── Database maintenance (`db` subcommand) ──────────────────────────────────

/// 1234567 → "1,234,567"
fn fmt_count(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{}", out) } else { out }
}

/// Stored UTC timestamp → local time for display
fn fmt_local(utc: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(utc, "%Y-%m-%d %H:%M:%S")
        .map(|t| {
            t.and_utc()
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| utc.to_string())
}

fn file_tables(conn: &Connection) -> DbResult<Vec<String>> {
    let mut stmt = conn.prepare(
        r"SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'files\_%' ESCAPE '\' ORDER BY name",
    )?;
    let names = stmt.query_map([], |row| row.get(0))?.collect();
    names
}

struct RootRow {
    root: String,
    first_run: String,
    last_started: String,
    last_finished: Option<String>,
    last_mode: String,
}

/// table name → its roots row (empty if the roots table doesn't exist yet)
fn load_roots(conn: &Connection) -> DbResult<HashMap<String, RootRow>> {
    if !table_exists(conn, "roots")? {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare(
        "SELECT table_name, root, first_run, last_started, last_finished, last_mode FROM roots",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                RootRow {
                    root: row.get(1)?,
                    first_run: row.get(2)?,
                    last_started: row.get(3)?,
                    last_finished: row.get(4)?,
                    last_mode: row.get(5)?,
                },
            ))
        })?
        .collect();
    rows
}

/// (duplicate groups, wasted bytes) across every row in the table. Hard links (same
/// device + inode) count as one copy; rows without an inode each count as their own copy.
fn table_duplicates(conn: &Connection, t: &str) -> DbResult<(i64, i64)> {
    // Tables v4 hasn't run against yet (e.g. left over from v3) have no inode column
    let has_inode: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{t}') WHERE name = 'inode')"),
        [],
        |row| row.get(0),
    )?;
    let copies = if has_inode {
        "COUNT(DISTINCT CASE WHEN inode IS NOT NULL THEN device || ':' || inode END) + SUM(inode IS NULL)"
    } else {
        "COUNT(*)"
    };
    conn.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(size * (copies - 1)), 0) FROM (
                 SELECT MAX(size) AS size, {copies} AS copies
                 FROM {t}
                 WHERE size > 0 AND checksum IS NOT NULL AND checksum != ''
                 GROUP BY checksum
             ) WHERE copies > 1"
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Bytes on disk used by a table and its indexes, if SQLite's dbstat is available
fn table_disk_bytes(conn: &Connection, t: &str) -> Option<i64> {
    conn.query_row(
        "SELECT SUM(pgsize) FROM dbstat
         WHERE name = ?1 OR name IN (SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1)",
        [t],
        |row| row.get(0),
    )
    .ok()
}

/// Main database file plus its WAL, in bytes
fn db_file_bytes(db_path: &str) -> u64 {
    let size = |p: &str| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    size(db_path) + size(&format!("{}-wal", db_path))
}

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

fn db_drop(conn: &Connection, path: Option<&str>, table: Option<&str>, yes: bool) -> Result<(), String> {
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

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("not running interactively — pass --yes to confirm".to_string());
        }
        eprint!("Remove the index of {} ({} files)? This can't be undone. [y/N] ", label, fmt_count(files));
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("cancelled".to_string());
        }
    }

    conn.execute(&format!("DROP TABLE {t}"), []).map_err(db)?;
    if table_exists(conn, "roots").map_err(db)? {
        conn.execute("DELETE FROM roots WHERE table_name = ?1", [&t]).map_err(db)?;
    }
    println!("removed the index of {} ({} files)", label, fmt_count(files));
    println!("run `diskunder db vacuum` to shrink the database file");
    Ok(())
}

fn db_vacuum(conn: &Connection, db_path: &str) -> DbResult<()> {
    let before = db_file_bytes(db_path);
    conn.execute_batch("VACUUM;")?;
    // Fold the WAL back in and truncate it, so the size reported is the real footprint
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let after = db_file_bytes(db_path);
    println!("vacuumed {}: {} → {}", db_path, fmt_bytes(before), fmt_bytes(after));
    Ok(())
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
fn run_db_command(db_path: &str, action: &DbAction) -> i32 {
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
        DbAction::Drop { path, table, yes } => db_drop(&conn, path.as_deref(), table.as_deref(), *yes),
        DbAction::Vacuum => db_vacuum(&conn, db_path).map_err(|e| e.to_string()),
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

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();

    let (mode, typed) = match &args.cmd {
        Command::Index { path } => ("index", path.clone()),
        Command::Update { path } => ("update", path.clone()),
        Command::Db { action } => std::process::exit(run_db_command(&args.db, action)),
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
    let db_path = args.db.clone();
    let threads = args.threads;

    // Use plain-text mode if --no-tui is set OR stdout is not an interactive terminal
    // (e.g. cron job, SSH without TTY, piped output)
    let plain = args.no_tui || !std::io::stdout().is_terminal();

    // Start worker on a background thread
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    let path_w = path.clone();
    let db_w = db_path.clone();
    std::thread::spawn(move || run_worker(path_w, typed, db_w, threads, upsert, tx));

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
        println!("  skipped       : {} ({})", state.files_skipped, state.skipped_label());
        println!("  files written : {}", state.files_written);
        println!("  rows removed  : {}", state.files_removed);
        println!("  errors        : {}", state.errors_summary());
        println!("  dup groups    : {}", state.dup_keys.len());
        if !state.dup_keys.is_empty() {
            println!("  wasted        : {}", fmt_bytes(state.dup_wasted));
        }
    }

    Ok(())
}
