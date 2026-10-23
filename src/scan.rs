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

use crate::db::{commit_batch, create_indexes, create_table, delete_paths, load_indexed, migrate_legacy_table, migrate_schema, roots_run_finished, roots_run_started, stored_filter, update_file_ids};
use crate::filters::{filter_notes, Overrides};
use crate::fmt::fmt_time;

const BATCH_SIZE: usize = 1_000;
const READ_CHUNK: usize = 65_536; // 64 KB

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
    pub(crate) fn from_path(path: &Path) -> Result<Self, std::io::Error> {
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

// ─── Worker thread ───────────────────────────────────────────────────────────

/// `path` is the canonical root; `typed` is the root as given on the command line,
/// used only to find a table created by an earlier version.
pub(crate) fn run_worker(
    path: String,
    typed: String,
    db_path: String,
    threads: usize,
    upsert: bool,
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
                    if let Some((p, _)) = indexed.remove_entry(e.path().to_string_lossy().as_ref()) {
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
            indexed.remove(&p);
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
