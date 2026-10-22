//! `index --dry-run` / `update --dry-run`: walk the tree and report what a real run would do —
//! without hashing, writing, migrating or logging anything

use rusqlite::Connection;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

use crate::db::{file_tables, schema_version, stored_filter, table_exists, table_name_for, SCHEMA_VERSION};
use crate::filters::{ext_of, TypeFilter};
use crate::fmt::{fmt_bytes, fmt_count, fmt_time};
use crate::output::{write_text_table, Align, Out};

/// path → (size, modified) for rows the real run would treat as already indexed
type Indexed = HashMap<String, (u64, String)>;

#[derive(Default)]
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

/// What the real run would start from: its rows, the file-type filters recorded for the path
/// (None if no runs are recorded), and notes about anything it would do first
fn load_indexed(db_path: &str, typed: &str, root: &str) -> Result<(Indexed, Option<TypeFilter>, Vec<String>), String> {
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

fn dry_run(db_path: &str, typed: &str, root: &str, upsert: bool, override_filter: Option<&TypeFilter>) -> Result<(), String> {
    let mode = if upsert { "update" } else { "index" };
    let (mut indexed, recorded, mut notes) = load_indexed(db_path, typed, root)?;
    // Same rule as the real run: command-line filters replace the stored ones
    let stored = recorded.clone().unwrap_or_default();
    let filter = override_filter.cloned().unwrap_or_else(|| stored.clone());
    if recorded.is_some() && filter != stored {
        notes.push(format!("file-type filters would change: {} → {}", stored.describe(), filter.describe()));
    } else if override_filter.is_none() && !filter.is_empty() {
        notes.push(format!("using the stored file-type filters: {}", filter.describe()));
    } else if !filter.is_empty() {
        notes.push(format!("file-type filters: {}", filter.describe()));
    }

    let mut found = Tally::default();
    let mut kept = 0i64; // already indexed (index) or unchanged (update)
    let mut new = Tally::default();
    let mut changed = Tally::default();
    let mut by_type: HashMap<String, Tally> = HashMap::new();
    let mut excluded = Tally::default();
    let mut excluded_by_type: HashMap<String, Tally> = HashMap::new();
    let mut now_excluded = 0i64; // indexed rows whose type the filters now exclude
    let mut unreadable: Option<Vec<PathBuf>> = Some(Vec::new()); // None: an error had no path
    let mut errors = 0i64;

    for entry in WalkDir::new(root) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let meta = e.metadata().ok();
                let size = meta.as_ref().map_or(0, |m| m.len());
                let key = e.path().to_string_lossy().into_owned();
                let ext = ext_of(e.path());
                if !filter.allows(&ext) {
                    excluded.add(size);
                    excluded_by_type.entry(ext).or_default().add(size);
                    if indexed.remove(&key).is_some() {
                        now_excluded += 1;
                    }
                    continue;
                }
                found.add(size);
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
                eprintln!(
                    "warn: {} — {}",
                    err.path().map(|p| p.display().to_string()).unwrap_or_default(),
                    err
                );
            }
        }
    }

    // What update would remove — with the real run's safeguards
    let mut removal_note = None;
    let mut remove = 0i64;
    if upsert && !indexed.is_empty() {
        match &unreadable {
            // Excluded files still show the share is there
            _ if found.files == 0 && excluded.files == 0 => {
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
    let mut summary = vec![
        ("Found".to_string(), format!("{} · {}", plural(found.files, "file"), fmt_bytes(found.bytes as u64))),
        (
            if upsert { "Unchanged" } else { "Already indexed" }.to_string(),
            plural(kept, "file"),
        ),
        (
            "Would hash".to_string(),
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
    if !filter.is_empty() {
        summary.push((
            "Excluded".to_string(),
            format!("{} · {}  ({})", plural(excluded.files, "file"), fmt_bytes(excluded.bytes as u64), filter.describe()),
        ));
    }
    if upsert {
        summary.push(("Would remove".to_string(), format!("{} (no longer on disk)", plural(remove, "row"))));
        if now_excluded > 0 {
            summary.push(("Now excluded".to_string(), format!("{} would be removed (type now excluded)", plural(now_excluded, "row"))));
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
    let mut out = Out::new();
    out.write(&format!("Dry run — {} {} (nothing hashed or written)\n", mode, root));
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

    write_types(&mut out, "Would hash, by type", by_type, hash.bytes);
    write_types(&mut out, "Excluded by filters, by type", excluded_by_type, excluded.bytes);
    out.flush();
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
pub(crate) fn run_dry_run(db_path: &str, typed: &str, root: &str, upsert: bool, filter: Option<&TypeFilter>) -> i32 {
    match dry_run(db_path, typed, root, upsert, filter) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}
