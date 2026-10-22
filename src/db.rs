//! SQLite schema, migrations and queries for index tables and the roots table

use rusqlite::{params, Connection, Result as DbResult};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

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
pub(crate) const SCHEMA_VERSION: i64 = 1;

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

    Ok(applied)
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
    last_mode     TEXT NOT NULL    -- index | update
)";

pub(crate) fn roots_run_started(conn: &Connection, table: &str, root: &str, mode: &str) -> DbResult<()> {
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
}

/// table name → its roots row (empty if the roots table doesn't exist yet)
pub(crate) fn load_roots(conn: &Connection) -> DbResult<HashMap<String, RootRow>> {
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
