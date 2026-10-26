//! `index --dry-run` / `update --dry-run`: walk the tree and report what a real run would do —
//! without hashing, writing, migrating or logging anything

use rusqlite::Connection;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

use crate::db::{file_tables, schema_version, stored_filter, table_exists, table_name_for, SCHEMA_VERSION};
use crate::filters::{ext_of, filter_notes, IndexFilter, Overrides};
use crate::fmt::{fmt_bytes, fmt_count, fmt_time};
use crate::output::{write_text_table, Align, Out};

/// path → (size, modified) for rows the real run would treat as already indexed
type Indexed = HashMap<String, (u64, String)>;

#[derive(Clone, Default)]
struct Tally {
    files: i64,
    bytes: i64,
}

impl Tally {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes as i64;
    }
}

fn load_rows(conn: &Connection, t: &str) -> rusqlite::Result<Vec<(String, u64, String)>> {
    let mut stmt = conn.prepare(&format!("SELECT path, size, COALESCE(modified_date, '') FROM {t}"))?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect();
    rows
}

/// What the real run would start from: its rows, the filters recorded for the path (None if
/// no runs are recorded), and notes about anything it would do first
fn load_indexed(db_path: &str, typed: &str, root: &str) -> Result<(Indexed, Option<IndexFilter>, Vec<String>), String> {
    let mut notes = Vec::new();
    if !Path::new(db_path).exists() {
        notes.push(format!("no database at {} yet — the run would create it", db_path));
        return Ok((Indexed::new(), None, notes));
    }
    let db = |e: rusqlite::Error| e.to_string();
    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;

    let version = schema_version(&conn).map_err(db)?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "database schema version {} is newer than this diskunder supports ({}) — upgrade diskunder",
            version, SCHEMA_VERSION
        ));
    }
    if version < SCHEMA_VERSION && !file_tables(&conn).map_err(db)?.is_empty() {
        notes.push(format!("the run would first update the database schema to version {}", SCHEMA_VERSION));
    }

    let table = table_name_for(root);
    if table_exists(&conn, &table).map_err(db)? {
        let rows = load_rows(&conn, &table).map_err(db)?;
        let recorded = stored_filter(&conn, &table).map_err(db)?;
        return Ok((rows.into_iter().map(|(p, s, m)| (p, (s, m))).collect(), recorded, notes));
    }

    // A v3 table named after the path as typed is migrated, not re-hashed — count its rows
    // as indexed, under the paths the migration would give them
    let legacy = table_name_for(typed);
    if legacy != table && table_exists(&conn, &legacy).map_err(db)? {
        let rows = load_rows(&conn, &legacy).map_err(db)?;
        notes.push(format!("the run would first migrate {} rows from the table for \"{}\"", fmt_count(rows.len() as i64), typed));
        let indexed = rows
            .into_iter()
            .filter_map(|(p, s, m)| {
                let rel = Path::new(&p).strip_prefix(typed).ok()?;
                let new_path = if rel.as_os_str().is_empty() { PathBuf::from(root) } else { Path::new(root).join(rel) };
                Some((new_path.to_string_lossy().into_owned(), (s, m)))
            })
            .collect();
        return Ok((indexed, None, notes));
    }

    notes.push(format!("{} isn't indexed in this database yet", root));
    Ok((Indexed::new(), None, notes))
}

fn plural(n: i64, word: &str) -> String {
    format!("{} {}{}", fmt_count(n), word, if n == 1 { "" } else { "s" })
}

/// Writes the report to `out`; traversal warnings go to `warnings`
fn dry_run(
    db_path: &str,
    typed: &str,
    root: &str,
    upsert: bool,
    hash_all: bool,
    overrides: &Overrides,
    out: &mut Out,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let mode = if upsert { "update" } else { "index" };
    let (mut indexed, recorded, mut notes) = load_indexed(db_path, typed, root)?;
    // Same rule as the real run: command-line filters replace stored ones of the same kind
    let filter = overrides.apply(&recorded.clone().unwrap_or_default());
    notes.extend(filter_notes(recorded.as_ref(), &filter, overrides, true));
    let root_path = PathBuf::from(root);

    let mut found = Tally::default();
    let mut kept = 0i64; // already indexed (index) or unchanged (update)
    let mut new = Tally::default();
    let mut changed = Tally::default();
    let mut by_type: HashMap<String, Tally> = HashMap::new();
    let mut excluded = Tally::default();
    let mut excluded_by_type: HashMap<String, Tally> = HashMap::new();
    let mut now_excluded = 0i64; // indexed rows the filters now exclude
    let mut sizes: HashMap<u64, i64> = HashMap::new(); // files found, by size
    let mut to_record: Vec<u64> = Vec::new(); // sizes of new and changed files
    let mut unreadable: Option<Vec<PathBuf>> = Some(Vec::new()); // None: an error had no path
    let mut errors = 0i64;
    let pruned = std::cell::Cell::new(0i64); // folders the walk wouldn't enter
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        let skip = e.depth() > 0 && e.file_type().is_dir() && filter.prunes_dir(e.path(), &root_path);
        if skip {
            pruned.set(pruned.get() + 1);
        }
        !skip
    });

    for entry in walker {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let meta = e.metadata().ok();
                let size = meta.as_ref().map_or(0, |m| m.len());
                let key = e.path().to_string_lossy().into_owned();
                let ext = ext_of(e.path());
                if filter.skips_file(e.path(), &root_path) {
                    excluded.add(size);
                    excluded_by_type.entry(ext).or_default().add(size);
                    if indexed.remove(&key).is_some() {
                        now_excluded += 1;
                    }
                    continue;
                }
                found.add(size);
                *sizes.entry(size).or_default() += 1;
                // Same rules as the real run: index keeps any existing row; update keeps a row
                // only if size and modified time still match
                let stored = indexed.remove(&key);
                let outcome = match stored {
                    Some(_) if !upsert => None,
                    Some((s, m)) => {
                        let modified = meta.as_ref().and_then(|m| m.modified().ok()).map(fmt_time).unwrap_or_default();
                        if s == size && m == modified { None } else { Some(&mut changed) }
                    }
                    None => Some(&mut new),
                };
                match outcome {
                    None => kept += 1,
                    Some(tally) => {
                        tally.add(size);
                        to_record.push(size);
                        by_type.entry(ext).or_default().add(size);
                    }
                }
            }
            Ok(_) => {}
            Err(err) => {
                errors += 1;
                match (err.path(), unreadable.as_mut()) {
                    (Some(p), Some(list)) => list.push(p.to_owned()),
                    _ => unreadable = None,
                }
                warnings.push(format!(
                    "{} — {}",
                    err.path().map(|p| p.display().to_string()).unwrap_or_default(),
                    err
                ));
            }
        }
    }

    // Rows under folders the walk would skip are excluded, not deleted
    if !filter.paths.is_empty() {
        let under_skipped: Vec<String> =
            indexed.keys().filter(|p| filter.excludes_row(p, &root_path)).cloned().collect();
        for p in under_skipped {
            indexed.remove(&p);
            now_excluded += 1;
        }
    }
    let pruned = pruned.get();

    // What update would remove — with the real run's safeguards
    let mut removal_note = None;
    let mut remove = 0i64;
    if upsert && !indexed.is_empty() {
        match &unreadable {
            // Excluded files and folders still show the share is there
            _ if found.files == 0 && excluded.files == 0 && pruned == 0 => {
                removal_note = Some(format!(
                    "nothing would be removed: no files found, which usually means an unmounted share ({} rows kept)",
                    fmt_count(indexed.len() as i64)
                ))
            }
            None => removal_note = Some("nothing would be removed: traversal errors without a path".to_string()),
            Some(skip_under) => {
                remove = indexed
                    .keys()
                    .filter(|p| !skip_under.iter().any(|d| Path::new(p).starts_with(d)))
                    .count() as i64;
            }
        }
    }

    let hash = Tally { files: new.files + changed.files, bytes: new.bytes + changed.bytes };
    let read = if hash_all {
        hash.clone()
    } else {
        // Files with a size another file has — in this walk or another indexed path. An upper
        // bound: big ones are read in full only if their quick hash matches too.
        let other = other_paths_sizes(db_path, root, &to_record);
        let mut t = Tally::default();
        for s in to_record.iter().filter(|s| **s > 0 && (sizes.get(s).copied().unwrap_or(0) > 1 || other.contains(s))) {
            t.add(*s);
        }
        t
    };
    let mut summary = vec![
        ("Found".to_string(), format!("{} · {}", plural(found.files, "file"), fmt_bytes(found.bytes as u64))),
        (
            if upsert { "Unchanged" } else { "Already indexed" }.to_string(),
            plural(kept, "file"),
        ),
        (
            "Would record".to_string(),
            if upsert {
                format!(
                    "{} · {}  ({} new, {} changed)",
                    plural(hash.files, "file"),
                    fmt_bytes(hash.bytes as u64),
                    fmt_count(new.files),
                    fmt_count(changed.files)
                )
            } else {
                format!("{} · {}", plural(hash.files, "file"), fmt_bytes(hash.bytes as u64))
            },
        ),
    ];
    summary.push((
        "Would read".to_string(),
        if hash_all {
            format!("{} · {}  (--hash-all)", plural(read.files, "file"), fmt_bytes(read.bytes as u64))
        } else {
            format!(
                "up to {} · {}  (their size matches another file's; the rest aren't read)",
                plural(read.files, "file"),
                fmt_bytes(read.bytes as u64)
            )
        },
    ));
    if !filter.types.is_empty() || !filter.paths.is_empty() {
        let folders = if pruned > 0 { format!(", {} not entered", plural(pruned, "folder")) } else { String::new() };
        summary.push((
            "Excluded".to_string(),
            format!("{} · {}{}", plural(excluded.files, "file"), fmt_bytes(excluded.bytes as u64), folders),
        ));
    }
    if upsert {
        summary.push(("Would remove".to_string(), format!("{} (no longer on disk)", plural(remove, "row"))));
        if now_excluded > 0 {
            summary.push(("Now excluded".to_string(), format!("{} would be removed (now excluded by filters)", plural(now_excluded, "row"))));
        }
    } else if now_excluded > 0 {
        notes.push(format!(
            "{} indexed files are now excluded — index keeps their rows; update would remove them",
            fmt_count(now_excluded)
        ));
    }
    if errors > 0 {
        summary.push((
            "Couldn't read".to_string(),
            format!("{} — rows under {} are kept", plural(errors, "folder"), if errors == 1 { "it" } else { "them" }),
        ));
    }
    out.write(&format!("Dry run — {} {} (nothing read or written)\n", mode, root));
    for note in &notes {
        out.write(&format!("  note: {}\n", note));
    }
    out.write("\n");
    for (label, value) in &summary {
        out.write(&format!("  {:<16} {}\n", label, value));
    }
    if let Some(note) = removal_note {
        out.write(&format!("  {}\n", note));
    }

    write_types(out, "Would record, by type", by_type, hash.bytes);
    write_types(out, "Excluded by filters, by type", excluded_by_type, excluded.bytes);
    Ok(())
}

fn write_types(out: &mut Out, title: &str, by_type: HashMap<String, Tally>, total: i64) {
    if !by_type.is_empty() {
        let mut types: Vec<(String, Tally)> = by_type.into_iter().collect();
        types.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)));
        let rows: Vec<Vec<String>> = types
            .iter()
            .map(|(ext, t)| {
                vec![
                    if ext.is_empty() { "(none)".to_string() } else { ext.clone() },
                    fmt_count(t.files),
                    fmt_bytes(t.bytes as u64),
                    format!("{:.1}%", if total > 0 { t.bytes as f64 * 100.0 / total as f64 } else { 0.0 }),
                ]
            })
            .collect();
        out.write(&format!("\n{}\n", title));
        write_text_table(
            out,
            &["TYPE", "FILES", "SIZE", "SHARE"],
            &[Align::Left, Align::Right, Align::Right, Align::Right],
            &rows,
        );
    }
}

/// `typed` is the path as given; `root` its canonical form
pub(crate) fn run_dry_run(db_path: &str, typed: &str, root: &str, upsert: bool, hash_all: bool, filter: &Overrides) -> i32 {
    let mut out = Out::new();
    let mut warnings = Vec::new();
    let result = dry_run(db_path, typed, root, upsert, hash_all, filter, &mut out, &mut warnings);
    for w in &warnings {
        eprintln!("warn: {}", w);
    }
    out.flush();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

/// Which of `sizes` some file in another indexed path of the database has
fn other_paths_sizes(db_path: &str, root: &str, sizes: &[u64]) -> std::collections::HashSet<u64> {
    let mut found = std::collections::HashSet::new();
    if !Path::new(db_path).exists() {
        return found;
    }
    let Ok(conn) = Connection::open(db_path) else { return found };
    let me = table_name_for(root);
    let distinct: std::collections::HashSet<u64> = sizes.iter().copied().collect();
    for t in file_tables(&conn).unwrap_or_default().into_iter().filter(|t| *t != me) {
        let Ok(mut stmt) = conn.prepare(&format!("SELECT 1 FROM {t} WHERE size = ?1 LIMIT 1")) else { continue };
        for s in &distinct {
            if !found.contains(s) && stmt.exists([*s as i64]).unwrap_or(false) {
                found.insert(*s);
            }
        }
    }
    found
}

/// The dry-run report as text, with any traversal warnings — for the interactive app
pub(crate) fn dry_run_report(
    db_path: &str,
    typed: &str,
    root: &str,
    upsert: bool,
    hash_all: bool,
    filter: &Overrides,
) -> Result<(String, Vec<String>), String> {
    let mut out = Out::buffer();
    let mut warnings = Vec::new();
    dry_run(db_path, typed, root, upsert, hash_all, filter, &mut out, &mut warnings)?;
    Ok((out.into_text(), warnings))
}
