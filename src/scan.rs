//! Walking, hashing and writing: the indexing worker thread

use rayon::prelude::*;
use rusqlite::Connection;
use std::{
    cell::Cell,
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::mpsc::Sender,
};
use walkdir::WalkDir;

use crate::db::{commit_batch, create_indexes, create_table, delete_paths, file_tables, has_column, load_indexed, load_roots, migrate_legacy_table, migrate_schema, roots_run_finished, roots_run_started, set_hashes, stored_filter, update_file_ids};
use crate::query::copy_id_expr;
use crate::filters::{filter_notes, Overrides};
use crate::fmt::fmt_time;

const BATCH_SIZE: usize = 1_000;
const READ_CHUNK: usize = 65_536; // 64 KB
/// Files up to this size that might have a copy are hashed in full straight away; bigger ones
/// get a quick hash (size, first and last 64 KB) first
const QUICK_MIN: u64 = 1 << 20; // 1 MB

/// (device, inode) — identifies the underlying file, so hard links to the same data
/// are recognised as one copy. None where the platform or filesystem doesn't provide it.
pub(crate) type FileId = (u64, u64);

#[cfg(unix)]
pub(crate) fn file_id(m: &fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}

#[cfg(not(unix))]
pub(crate) fn file_id(_: &fs::Metadata) -> Option<FileId> {
    None
}

// ─── Worker → UI messages ────────────────────────────────────────────────────

pub(crate) enum WorkerMsg {
    /// One file discovered during directory traversal
    FileFound,
    /// One new or changed file's details recorded (not read)
    FileRecorded { path: String },
    /// A step of the run begins, over `files` files: "record", "quick" (quick hashes of files
    /// whose size another file shares) or "full" (full hashes of files that may be copies)
    Stage { name: &'static str, files: usize },
    /// One quick hash done
    QuickChecked { path: String },
    /// Comparing finished: this path's files left unread (no other file shares their size,
    /// or their first and last 64 KB)
    Compared { unread: usize },
    /// One file hashed in full
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
    /// An error that stopped the run before it could index anything; `Done` follows
    Fatal { context: String, error: String },
    /// Informational line for the activity log (also printed in plain mode)
    Info(String),
    /// A batch of N rows committed to SQLite
    BatchWritten(usize),
    /// N rows removed for files no longer on disk (update mode)
    Removed(usize),
    /// Files skipped during the walk by the filters, and folders not entered at all
    Excluded { files: usize, folders: usize },
    /// N rows removed because their type is now excluded (update mode)
    RemovedExcluded(usize),
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

// ─── File entry ──────────────────────────────────────────────────────────────

pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) filename: String,
    pub(crate) size: u64,
    pub(crate) modified_date: String,
    pub(crate) access_date: String,
    pub(crate) birth_date: String,
    pub(crate) checksum: String,
    pub(crate) extension: String,
    pub(crate) directory: String,
    pub(crate) file_id: Option<FileId>,
}

impl FileEntry {
    /// A file's details, without reading it — hashing comes later, and only if it might have
    /// a copy
    pub(crate) fn from_path(path: &Path) -> Result<Self, std::io::Error> {
        let metadata = fs::metadata(path)?;
        let checksum = String::new();

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
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default(),
            directory: path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            file_id: file_id(&metadata),
        })
    }
}

/// Open for reading without updating the file's access time where Linux allows it (files the
/// user owns, or as root); otherwise a plain open
fn open_for_reading(path: &Path) -> Result<File, std::io::Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(f) = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOATIME).open(path) {
            return Ok(f);
        }
    }
    File::open(path)
}

/// The size and the first and last 64 KB: files that differ here aren't copies, so only
/// files whose quick hashes match need reading in full
fn quick_hash(path: &Path, size: u64) -> Result<String, std::io::Error> {
    use std::io::{Seek, SeekFrom};
    let mut f = open_for_reading(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let mut buf = vec![0u8; READ_CHUNK];
    for start in [0, size.saturating_sub(READ_CHUNK as u64)] {
        f.seek(SeekFrom::Start(start))?;
        let mut filled = 0;
        while filled < buf.len() {
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        hasher.update(&buf[..filled]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_streaming(path: &Path) -> Result<String, std::io::Error> {
    let mut reader = BufReader::with_capacity(READ_CHUNK, open_for_reading(path)?);
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

// ─── Worker thread ───────────────────────────────────────────────────────────

/// `path` is the canonical root; `typed` is the root as given on the command line,
/// used only to find a table created by an earlier version.
pub(crate) fn run_worker(
    path: String,
    typed: String,
    db_path: String,
    threads: usize,
    upsert: bool,
    hash_all: bool,       // checksum every file, not just possible copies
    overrides: Overrides, // filters from the command line; kinds not given reuse the stored ones
    tx: Sender<WorkerMsg>,
) {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let mut conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(WorkerMsg::Fatal { context: db_path, error: e.to_string() });
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
            let _ = tx.send(WorkerMsg::Fatal {
                context: "db".to_string(),
                error: format!("could not migrate table for \"{}\": {}", typed, e),
            });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    }

    match migrate_schema(&mut conn) {
        Ok(applied) => {
            for line in applied {
                let _ = tx.send(WorkerMsg::Info(line));
            }
        }
        Err(e) => {
            let _ = tx.send(WorkerMsg::Fatal {
                context: "db".to_string(),
                error: format!("could not update database schema: {}", e),
            });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    }

    let table_name = match create_table(&conn, &path) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(WorkerMsg::Fatal { context: path, error: e.to_string() });
            let _ = tx.send(WorkerMsg::Done);
            return;
        }
    };
    // Filters: the command line's replace the stored ones of the same kind; otherwise reuse
    let recorded = stored_filter(&conn, &table_name).ok().flatten(); // None: no runs recorded yet
    let filter = overrides.apply(&recorded.clone().unwrap_or_default());
    for note in filter_notes(recorded.as_ref(), &filter, &overrides, false) {
        let _ = tx.send(WorkerMsg::Info(note));
    }
    let root = PathBuf::from(&path);

    let mode = if upsert { "update" } else { "index" };
    let volume = crate::volume::volume_of(&root);
    let volume_changed = load_roots(&conn).ok().and_then(|r| r.get(&table_name).map(|r| r.volume.clone())).flatten() != volume;
    if let Some(v) = &volume {
        // Another indexed path over the same files, e.g. the same share mounted elsewhere
        for (t, r) in crate::db::load_roots(&conn).unwrap_or_default() {
            if t != table_name && r.volume.as_ref().is_some_and(|o| crate::volume::overlaps(o, v)) {
                let _ = tx.send(WorkerMsg::Info(format!(
                    "note: {} overlaps {}, already indexed here (same volume) — duplicates count shared files once",
                    path, r.root
                )));
            }
        }
    }
    if let Err(e) = roots_run_started(&conn, &table_name, &path, mode, &filter, volume.as_ref()) {
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
    let mut excluded = 0usize; // files skipped by the filters (never stat'ed or hashed)
    let mut now_excluded: Vec<String> = Vec::new(); // indexed rows the filters now exclude
    // Checksums whose copies this run may change, for the duplicate summary: of rows changed,
    // removed or newly hashed (and all this path's if its volume identity changed)
    let mut affected: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Excluded folders are never entered, unless an include pattern could match inside them
    let pruned = Cell::new(0usize);
    let walker = WalkDir::new(&path).into_iter().filter_entry(|e| {
        let skip = e.depth() > 0 && e.file_type().is_dir() && filter.prunes_dir(e.path(), &root);
        if skip {
            pruned.set(pruned.get() + 1);
        }
        !skip
    });

    for entry in walker {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                if filter.skips_file(e.path(), &root) {
                    excluded += 1;
                    if let Some((p, st)) = indexed.remove_entry(e.path().to_string_lossy().as_ref()) {
                        affected.insert(st.checksum);
                        now_excluded.push(p);
                    }
                    continue;
                }
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
                        if !unchanged {
                            affected.insert(s.checksum.clone()); // re-recorded without it
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

    // Rows under folders the walk skipped weren't seen, but they're excluded, not deleted
    if !filter.paths.is_empty() {
        let under_skipped: Vec<String> =
            indexed.keys().filter(|p| filter.excludes_row(p, &root)).cloned().collect();
        for p in under_skipped {
            if let Some(st) = indexed.remove(&p) {
                affected.insert(st.checksum);
            }
            now_excluded.push(p);
        }
    }

    // Excluded files and folders still prove the share is there, for the empty-walk safeguard
    let pruned = pruned.get();
    let found_any = !files.is_empty() || skipped > 0 || excluded > 0 || pruned > 0;
    if excluded > 0 || pruned > 0 {
        let _ = tx.send(WorkerMsg::Excluded { files: excluded, folders: pruned });
    }
    let _ = tx.send(WorkerMsg::TraversalComplete { to_hash: files.len(), skipped });

    // ── Rows for files that still exist but whose type is now excluded ──
    if !now_excluded.is_empty() {
        if upsert {
            match delete_paths(&mut conn, &table_name, &now_excluded) {
                Ok(n) => {
                    let _ = tx.send(WorkerMsg::RemovedExcluded(n));
                }
                Err(e) => {
                    let _ = tx.send(WorkerMsg::Error {
                        context: "db".to_string(),
                        error: format!("could not remove rows now excluded by filters: {}", e),
                    });
                }
            }
        } else {
            // index never removes rows
            let _ = tx.send(WorkerMsg::Info(format!(
                "{} indexed files are now excluded by the filters — `update` removes their rows",
                now_excluded.len()
            )));
        }
    }
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
                    .into_iter()
                    .filter(|(p, _)| !skip_under.iter().any(|d| Path::new(p).starts_with(d)))
                    .map(|(p, st)| {
                        affected.insert(st.checksum);
                        p
                    })
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

    // ── New and changed files recorded in batches (details only — nothing is read yet) ──
    let _ = tx.send(WorkerMsg::Stage { name: "record", files: files.len() });
    for chunk in files.chunks(BATCH_SIZE) {
        let results: Vec<Result<FileEntry, (String, String)>> = chunk
            .par_iter()
            .map(|p| FileEntry::from_path(p).map_err(|e| (p.display().to_string(), e.to_string())))
            .collect();

        let mut batch: Vec<FileEntry> = Vec::with_capacity(results.len());
        for result in results {
            match result {
                Ok(entry) => {
                    let _ = tx.send(WorkerMsg::FileRecorded { path: entry.path.clone() });
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
    // whose first run was `update`. Before comparing, which looks sizes up.
    if let Err(e) = create_indexes(&conn, &table_name) {
        let _ = tx.send(WorkerMsg::Error {
            context: "db".to_string(),
            error: format!("could not create indexes: {}", e),
        });
    }

    match compare(&mut conn, &table_name, hash_all, &tx) {
        Ok((unread, hashed)) => {
            let _ = tx.send(WorkerMsg::Compared { unread: unread as usize });
            affected.extend(hashed);
        }
        Err(e) => {
            let _ = tx.send(WorkerMsg::Error {
                context: "db".to_string(),
                error: format!("could not compare files for duplicates: {}", e),
            });
        }
    }

    // The duplicate summary: built once, then kept up to date with what this run changed
    let summary_started = std::time::Instant::now();
    let summary = match crate::db::has_dup_groups(&conn) {
        Ok(true) => {
            if volume_changed {
                affected.extend(crate::db::table_checksums(&conn, &table_name).unwrap_or_default());
            }
            crate::db::refresh_dup_groups(&conn, &affected).map(|_| None)
        }
        _ => crate::db::rebuild_dup_groups(&conn).map(Some),
    };
    match summary {
        // Worth a mention when upgrading a big database; instant (and unremarkable) when new
        Ok(Some(n)) if summary_started.elapsed().as_secs() >= 2 => {
            let _ = tx.send(WorkerMsg::Info(format!(
                "built the duplicate summary in {}: {} groups (once per database)",
                crate::jobs::fmt_secs(summary_started.elapsed().as_secs() as i64),
                n
            )));
        }
        Ok(Some(_)) => {}
        Ok(None) => {}
        Err(e) => {
            let _ = tx.send(WorkerMsg::Error {
                context: "db".to_string(),
                error: format!("could not update the duplicate summary: {}", e),
            });
        }
    }

    if let Err(e) = roots_run_finished(&conn, &table_name) {
        let _ = tx.send(WorkerMsg::Error {
            context: "db".to_string(),
            error: format!("could not record run in roots table: {}", e),
        });
    }

    let _ = tx.send(WorkerMsg::Done);
}

// ─── Comparing: which files to read ─────────────────────────────────────────

/// A file that might be a copy: (table, path, size, quick hash, device and inode)
struct Target {
    table: String,
    path: String,
    size: u64,
    quick: Option<String>,
    file_id: Option<FileId>,
}

/// Hash what might be a copy, and nothing else. A file is read only if another file — in
/// any indexed path of this database — has its size: in full if it's small; otherwise a
/// quick hash first, and in full only if that matches another file's quick hash (or a file
/// hashed before quick hashes existed). Files in other indexed paths that this path's files
/// could be copies of are read too, if they're reachable from here and unchanged; otherwise
/// they wait for their own path's next run. `hash_all` hashes every file of this path instead.
/// Returns how many of this path's files are left unread, and the checksums it found.
fn compare(conn: &mut Connection, table: &str, hash_all: bool, tx: &Sender<WorkerMsg>) -> rusqlite::Result<(i64, Vec<String>)> {
    let roots = load_roots(conn)?;
    let mut ids: HashMap<String, (String, String)> = HashMap::new(); // table → copy id as x. / t.
    let tables = file_tables(conn)?;
    for t in &tables {
        let volume = roots.get(t).and_then(|r| r.volume.as_ref()).map(|v| v.id.clone());
        let inode = has_column(conn, t, "inode")?;
        ids.insert(t.clone(), (copy_id_expr("x.", volume.as_deref(), inode), copy_id_expr("t.", volume.as_deref(), inode)));
    }
    let inode_cols = |t: &str| -> rusqlite::Result<&'static str> {
        Ok(if has_column(conn, t, "inode")? { "t.device, t.inode" } else { "NULL, NULL" })
    };
    let mut selects = HashMap::new();
    for t in &tables {
        selects.insert(t.clone(), format!("SELECT t.path, t.size, t.quick, {}, t.modified_date FROM {t} t", inode_cols(t)?));
    }
    // Another file than `t` (not it, a hard link to it, or it seen through another indexed
    // path) of the same size in any of `among`, that also meets `cond`
    let another = |of: &str, among: &[String], cond: &str| -> String {
        among
            .iter()
            .map(|o| format!("EXISTS (SELECT 1 FROM {o} x WHERE x.size = t.size AND {} != {} AND {cond})", ids[o].0, ids[of].1))
            .collect::<Vec<_>>()
            .join(" OR ")
    };
    let me = [table.to_string()];

    let mut targets: Vec<Target> = if hash_all {
        candidate_rows(conn, table, &format!("{} WHERE t.checksum IS NULL", selects[table]), false)?
    } else {
        candidate_rows(conn, table, &format!("{} WHERE t.checksum IS NULL AND t.size > 0 AND ({})", selects[table], another(table, &tables, "1")), false)?
    };
    if !hash_all {
        // Unread files in other paths with the size of one of this path's files
        // Driven by this path's sizes, each looked up in the other path's size index — not by
        // the other path's unread rows, which can be millions
        for o in tables.iter().filter(|o| *o != table) {
            let sql = format!(
                "SELECT t.path, t.size, t.quick, {}, t.modified_date
                 FROM (SELECT DISTINCT size AS s FROM {table} WHERE size > 0) m CROSS JOIN {o} t ON t.size = m.s
                 WHERE t.checksum IS NULL AND ({})",
                inode_cols(o)?,
                another(o, &me, "1")
            );
            targets.extend(candidate_rows(conn, o, &sql, true)?);
        }
    }
    let in_targets: std::collections::HashSet<(String, String)> = targets.iter().map(|t| (t.table.clone(), t.path.clone())).collect();
    let (mut full, large): (Vec<Target>, Vec<Target>) = targets.into_iter().partition(|c| hash_all || c.size <= QUICK_MIN);

    // Quick hashes for big files that don't have one yet
    let need_quick: Vec<&Target> = large.iter().filter(|c| c.quick.is_none()).collect();
    if !need_quick.is_empty() {
        let _ = tx.send(WorkerMsg::Stage { name: "quick", files: need_quick.len() });
        for chunk in need_quick.chunks(BATCH_SIZE) {
            let done: Vec<(String, String, String)> = chunk
                .par_iter()
                .filter_map(|c| match quick_hash(Path::new(&c.path), c.size) {
                    Ok(q) => {
                        let _ = tx.send(WorkerMsg::QuickChecked { path: c.path.clone() });
                        Some((c.table.clone(), c.path.clone(), q))
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::FileError { path: c.path.clone(), error: e.to_string() });
                        None
                    }
                })
                .collect();
            write_hashes(conn, "quick", done)?;
        }
    }
    // Big files whose quick hash matches another's, or that share a size with a file hashed
    // before quick hashes existed
    if !hash_all && !large.is_empty() {
        let mut involved: Vec<&String> = large.iter().map(|c| &c.table).collect();
        involved.sort();
        involved.dedup();
        for t in involved {
            let sql = format!(
                "{} WHERE t.checksum IS NULL AND t.size > {QUICK_MIN} AND t.quick IS NOT NULL AND ({})",
                selects[t],
                another(t, &tables, "(x.quick = t.quick OR (x.quick IS NULL AND x.checksum IS NOT NULL))")
            );
            full.extend(candidate_rows(conn, t, &sql, false)?.into_iter().filter(|c| in_targets.contains(&(c.table.clone(), c.path.clone()))));
        }
    }

    let mut hashed = Vec::new();
    if !full.is_empty() {
        let _ = tx.send(WorkerMsg::Stage { name: "full", files: full.len() });
        for chunk in full.chunks(BATCH_SIZE) {
            let done: Vec<(String, String, String)> = chunk
                .par_iter()
                .filter_map(|c| match hash_streaming(Path::new(&c.path)) {
                    Ok(sum) => {
                        let _ = tx.send(WorkerMsg::FileHashed {
                            path: c.path.clone(),
                            size: c.size,
                            checksum: sum.clone(),
                            file_id: c.file_id,
                        });
                        Some((c.table.clone(), c.path.clone(), sum))
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::FileError { path: c.path.clone(), error: e.to_string() });
                        None
                    }
                })
                .collect();
            hashed.extend(done.iter().map(|(_, _, sum)| sum.clone()));
            write_hashes(conn, "checksum", done)?;
        }
    }

    let unread: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE checksum IS NULL AND size > 0"), [], |r| r.get(0))?;
    Ok((unread, hashed))
}

/// Store (table, path, hash) results in `column`, table by table
fn write_hashes(conn: &mut Connection, column: &str, done: Vec<(String, String, String)>) -> rusqlite::Result<()> {
    let mut by_table: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (t, path, hash) in done {
        by_table.entry(t).or_default().push((path, hash));
    }
    for (t, rows) in by_table {
        set_hashes(conn, &t, column, &rows)?;
    }
    Ok(())
}

/// The rows `sql` selects (path, size, quick, device, inode, modified) from `table`. With
/// `on_disk`, only files still there unchanged — rows of other paths may be stale, or on a
/// share this machine can't reach.
fn candidate_rows(conn: &Connection, table: &str, sql: &str, on_disk: bool) -> rusqlite::Result<Vec<Target>> {
    let mut stmt = conn.prepare(sql)?;
    let rows: Vec<(Target, String)> = stmt
        .query_map([], |r| {
            let (d, i): (Option<i64>, Option<i64>) = (r.get(3)?, r.get(4)?);
            Ok((
                Target {
                    table: table.to_string(),
                    path: r.get(0)?,
                    size: r.get(1)?,
                    quick: r.get(2)?,
                    file_id: d.zip(i).map(|(d, i)| (d as u64, i as u64)),
                },
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|(c, modified)| {
            !on_disk
                || fs::metadata(&c.path).is_ok_and(|m| m.len() == c.size && m.modified().ok().map(fmt_time).unwrap_or_default() == *modified)
        })
        .map(|(c, _)| c)
        .collect())
}
