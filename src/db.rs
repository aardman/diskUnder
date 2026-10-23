//! SQLite schema, migrations and queries for index tables and the roots table

use crate::volume::Volume;
use rusqlite::{params, Connection, OptionalExtension, Result as DbResult};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::filters::{IndexFilter, PathFilter, TypeFilter};
use crate::scan::{FileEntry, FileId};

pub(crate) fn table_name_for(root: &str) -> String {
    format!("files_{:x}", md5::compute(root))
}

pub(crate) fn table_exists(conn: &Connection, t: &str) -> DbResult<bool> {
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
pub(crate) fn migrate_legacy_table(conn: &mut Connection, typed: &str, root: &str) -> Result<Option<usize>, String> {
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

/// Layout version this build writes, kept in SQLite's `user_version`. Each step below runs
/// once, in order, over the whole database.
///   0 — v4.1 and earlier
///   1 — v5.0: file extensions stored lowercase
///   2 — v5.2: file-type filter columns on `roots`
///   3 — v5.3: path filter columns on `roots`
///   4 — v5.6: volume identity columns on `roots`
pub(crate) const SCHEMA_VERSION: i64 = 4;

pub(crate) fn schema_version(conn: &Connection) -> DbResult<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// Bring the database up to SCHEMA_VERSION. Returns a line describing each step that changed
/// anything, for the activity log. Refuses a database written by a newer diskunder.
pub(crate) fn migrate_schema(conn: &mut Connection) -> Result<Vec<String>, String> {
    let db = |e: rusqlite::Error| e.to_string();
    let version = schema_version(conn).map_err(db)?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "database schema version {} is newer than this diskunder supports ({}) — upgrade diskunder",
            version, SCHEMA_VERSION
        ));
    }
    let mut applied = Vec::new();

    if version < 1 {
        let tx = conn.transaction().map_err(db)?;
        let mut changed = 0;
        for t in file_tables(&tx).map_err(db)? {
            changed += tx
                .execute(
                    &format!("UPDATE {t} SET extension = lower(extension) WHERE extension != lower(extension)"),
                    [],
                )
                .map_err(db)?;
        }
        tx.pragma_update(None, "user_version", 1).map_err(db)?;
        tx.commit().map_err(db)?;
        if changed > 0 {
            applied.push(format!("schema 1: stored {} file extensions in lowercase", changed));
        }
    }

    if version < 2 {
        let tx = conn.transaction().map_err(db)?;
        let mut added = false;
        // A database with no runs recorded yet gets these columns when `roots` is created
        if table_exists(&tx, "roots").map_err(db)? {
            for col in ["include_ext", "exclude_ext"] {
                if !has_column(&tx, "roots", col).map_err(db)? {
                    tx.execute(&format!("ALTER TABLE roots ADD COLUMN {col} TEXT"), []).map_err(db)?;
                    added = true;
                }
            }
        }
        tx.pragma_update(None, "user_version", 2).map_err(db)?;
        tx.commit().map_err(db)?;
        if added {
            applied.push("schema 2: added file-type filter columns to the roots table".to_string());
        }
    }

    if version < 3 {
        let tx = conn.transaction().map_err(db)?;
        let mut added = false;
        if table_exists(&tx, "roots").map_err(db)? {
            for col in ["exclude_paths", "include_paths"] {
                if !has_column(&tx, "roots", col).map_err(db)? {
                    tx.execute(&format!("ALTER TABLE roots ADD COLUMN {col} TEXT"), []).map_err(db)?;
                    added = true;
                }
            }
        }
        tx.pragma_update(None, "user_version", 3).map_err(db)?;
        tx.commit().map_err(db)?;
        if added {
            applied.push("schema 3: added path filter columns to the roots table".to_string());
        }
    }

    if version < 4 {
        let tx = conn.transaction().map_err(db)?;
        let mut added = false;
        if table_exists(&tx, "roots").map_err(db)? {
            for col in ["volume", "volume_path"] {
                if !has_column(&tx, "roots", col).map_err(db)? {
                    tx.execute(&format!("ALTER TABLE roots ADD COLUMN {col} TEXT"), []).map_err(db)?;
                    added = true;
                }
            }
        }
        tx.pragma_update(None, "user_version", 4).map_err(db)?;
        tx.commit().map_err(db)?;
        if added {
            applied.push("schema 4: added volume columns to the roots table".to_string());
        }
    }

    Ok(applied)
}

pub(crate) fn has_column(conn: &Connection, t: &str, col: &str) -> DbResult<bool> {
    conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{t}') WHERE name = '{col}')"),
        [],
        |row| row.get(0),
    )
}

pub(crate) fn create_table(conn: &Connection, path: &str) -> DbResult<String> {
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
    last_mode     TEXT NOT NULL,   -- index | update
    include_ext   TEXT,            -- file-type filters: comma-separated, \"none\" = no extension
    exclude_ext   TEXT,
    exclude_paths TEXT,            -- path patterns, one per line
    include_paths TEXT,            -- exceptions to exclude_paths
    volume        TEXT,            -- the volume it's on, however it's mounted: smb://server/share
    volume_path   TEXT             -- where the path is inside that volume
)";

/// Records the run's start, and the filters it uses (so `update` can reuse them)
pub(crate) fn roots_run_started(
    conn: &Connection,
    table: &str,
    root: &str,
    mode: &str,
    filter: &IndexFilter,
    volume: Option<&Volume>,
) -> DbResult<()> {
    conn.execute(ROOTS_SCHEMA, [])?;
    let (include_ext, exclude_ext) = filter.types.to_db();
    let (exclude_paths, include_paths) = filter.paths.to_db();
    conn.execute(
        "INSERT INTO roots (table_name, root, first_run, last_started, last_finished, last_mode,
                            include_ext, exclude_ext, exclude_paths, include_paths, volume, volume_path)
         VALUES (?1, ?2, datetime('now'), datetime('now'), NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(table_name) DO UPDATE SET
           root = excluded.root, last_started = excluded.last_started,
           last_finished = NULL, last_mode = excluded.last_mode,
           include_ext = excluded.include_ext, exclude_ext = excluded.exclude_ext,
           exclude_paths = excluded.exclude_paths, include_paths = excluded.include_paths,
           volume = excluded.volume, volume_path = excluded.volume_path",
        params![
            table,
            root,
            mode,
            include_ext,
            exclude_ext,
            exclude_paths,
            include_paths,
            volume.map(|v| &v.id),
            volume.map(|v| &v.path)
        ],
    )?;
    Ok(())
}

/// Filter columns to select from `roots`: NULL for any that an older, un-migrated database
/// doesn't have yet (read-only commands don't migrate)
fn filter_columns(conn: &Connection) -> DbResult<String> {
    let mut cols = Vec::new();
    for col in ["include_ext", "exclude_ext", "exclude_paths", "include_paths"] {
        cols.push(if has_column(conn, "roots", col)? { col } else { "NULL" });
    }
    Ok(cols.join(", "))
}

fn filter_from_row(r: &rusqlite::Row, first: usize) -> DbResult<IndexFilter> {
    Ok(IndexFilter {
        types: TypeFilter::from_db(r.get(first)?, r.get(first + 1)?),
        paths: PathFilter::from_db(r.get(first + 2)?, r.get(first + 3)?),
    })
}

/// The filters stored for a table, if it has a roots row
pub(crate) fn stored_filter(conn: &Connection, table: &str) -> DbResult<Option<IndexFilter>> {
    if !table_exists(conn, "roots")? {
        return Ok(None);
    }
    conn.query_row(
        &format!("SELECT {} FROM roots WHERE table_name = ?1", filter_columns(conn)?),
        [table],
        |r| filter_from_row(r, 0),
    )
    .optional()
}

pub(crate) fn roots_run_finished(conn: &Connection, table: &str) -> DbResult<()> {
    conn.execute(
        "UPDATE roots SET last_finished = datetime('now') WHERE table_name = ?1",
        [table],
    )?;
    Ok(())
}

pub(crate) struct Stored {
    pub(crate) size: u64,
    pub(crate) modified: String,
    pub(crate) checksum: String,
    pub(crate) file_id: Option<FileId>,
}

// SQLite integers are signed 64-bit; device and inode numbers are stored bit-for-bit
fn id_to_db(id: Option<FileId>) -> (Option<i64>, Option<i64>) {
    (id.map(|(d, _)| d as i64), id.map(|(_, i)| i as i64))
}

fn id_from_db(device: Option<i64>, inode: Option<i64>) -> Option<FileId> {
    Some((device? as u64, inode? as u64))
}

/// path → stored size, modified date and checksum for every row already in the table
pub(crate) fn load_indexed(conn: &Connection, t: &str) -> DbResult<HashMap<String, Stored>> {
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
pub(crate) fn update_file_ids(conn: &mut Connection, t: &str, rows: &[(String, Option<FileId>)]) -> DbResult<usize> {
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

pub(crate) fn delete_paths(conn: &mut Connection, t: &str, paths: &[String]) -> DbResult<usize> {
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

pub(crate) fn create_indexes(conn: &Connection, t: &str) -> DbResult<()> {
    for col in &["size", "filename", "checksum", "extension", "directory", "access_date", "modified_date"] {
        conn.execute(
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_{c} ON {t}({c})", t = t, c = col),
            [],
        )?;
    }
    Ok(())
}

pub(crate) fn commit_batch(conn: &mut Connection, t: &str, batch: &[FileEntry], upsert: bool) -> DbResult<usize> {
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

pub(crate) fn file_tables(conn: &Connection) -> DbResult<Vec<String>> {
    let mut stmt = conn.prepare(
        r"SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'files\_%' ESCAPE '\' ORDER BY name",
    )?;
    let names = stmt.query_map([], |row| row.get(0))?.collect();
    names
}

pub(crate) struct RootRow {
    pub(crate) root: String,
    pub(crate) first_run: String,
    pub(crate) last_started: String,
    pub(crate) last_finished: Option<String>,
    pub(crate) last_mode: String,
    pub(crate) filter: IndexFilter,
    pub(crate) volume: Option<Volume>, // None until a v5.6 run records it
}

/// table name → its roots row (empty if the roots table doesn't exist yet)
pub(crate) fn load_roots(conn: &Connection) -> DbResult<HashMap<String, RootRow>> {
    if !table_exists(conn, "roots")? {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT table_name, root, first_run, last_started, last_finished, last_mode, {}, {} FROM roots",
        filter_columns(conn)?,
        if has_column(conn, "roots", "volume")? { "volume, volume_path" } else { "NULL, NULL" }
    ))?;
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
                    filter: filter_from_row(row, 6)?,
                    volume: match (row.get::<_, Option<String>>(10)?, row.get::<_, Option<String>>(11)?) {
                        (Some(id), Some(path)) => Some(Volume { id, path }),
                        _ => None,
                    },
                },
            ))
        })?
        .collect();
    rows
}

/// (duplicate groups, wasted bytes) across every row in the table. Hard links (same
/// device + inode) count as one copy; rows without an inode each count as their own copy.
pub(crate) fn table_duplicates(conn: &Connection, t: &str) -> DbResult<(i64, i64)> {
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
pub(crate) fn table_disk_bytes(conn: &Connection, t: &str) -> Option<i64> {
    conn.query_row(
        "SELECT SUM(pgsize) FROM dbstat
         WHERE name = ?1 OR name IN (SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1)",
        [t],
        |row| row.get(0),
    )
    .ok()
}

/// Main database file plus its WAL, in bytes
pub(crate) fn db_file_bytes(db_path: &str) -> u64 {
    let size = |p: &str| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    size(db_path) + size(&format!("{}-wal", db_path))
}

/// Held while something writes to a database — an index or update run, a drop, a vacuum — so a
/// second one is refused with who has it, instead of failing part-way with "database is locked".
/// The lock is the operating system's on `<db>.lock`: released when this is dropped or the
/// process ends, however it ends.
pub(crate) struct DbLock {
    _file: fs::File,
}

pub(crate) fn lock_db(db: &str, what: &str) -> Result<DbLock, String> {
    use std::io::{Read, Seek, Write};
    let path = format!("{}.lock", db);
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("can't create {}: {}", path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock on a file descriptor we own
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            let holder = holder.trim();
            return Err(format!(
                "{} is in use{} — one run at a time per database",
                db,
                if holder.is_empty() { String::new() } else { format!(" ({})", holder) }
            ));
        }
    }
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(
        file,
        "{} — pid {} on {}, since {}\n",
        what,
        std::process::id(),
        crate::runlog::host(),
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );
    Ok(DbLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_writer_per_database() {
        let dir = std::env::temp_dir().join(format!("diskunder-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db").to_string_lossy().into_owned();
        let first = lock_db(&db, "diskunder index /nas").unwrap();
        let err = lock_db(&db, "diskunder db vacuum").err().expect("a second writer is refused");
        assert!(err.contains("is in use (diskunder index /nas — pid") && err.contains("one run at a time per database"), "{}", err);
        drop(first);
        assert!(lock_db(&db, "diskunder db vacuum").is_ok(), "free once the first is done");
        let _ = fs::remove_dir_all(&dir);
    }
}
