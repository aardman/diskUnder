//! Quarantine: move files out of the way without deleting them. A file goes to
//! `<indexed root>/.diskunder-quarantine/<batch>/<its path inside the root>` — a rename on
//! the same volume, so nothing is copied — and its row moves from the files table to
//! `quarantined`, so reports and duplicate counts stop including it. `restore` puts it back;
//! `purge` deletes it for good once it has been there long enough.

use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

use crate::db::{has_column, load_roots, lock_db, migrate_schema, refresh_dup_groups, table_exists, table_name_for};
use crate::fmt::{fmt_bytes, fmt_count, fmt_local, fmt_time};
use crate::output::{write_text_table, Align, Out};
use crate::runlog::RunLog;
use crate::settings::Settings;

/// The folder at the top of each indexed path that holds its quarantined files; runs never
/// look inside it
pub(crate) const FOLDER: &str = ".diskunder-quarantine";
const MANIFEST: &str = "manifest.tsv";

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS quarantine_batches (
    batch    TEXT PRIMARY KEY,   -- e.g. 2026-11-16-153201
    created  TEXT NOT NULL,      -- UTC
    reason   TEXT,
    command  TEXT
);
CREATE TABLE IF NOT EXISTS quarantined (
    id            INTEGER PRIMARY KEY,
    batch         TEXT NOT NULL,
    root          TEXT NOT NULL,   -- the indexed path it belonged to
    original      TEXT NOT NULL,   -- where it was
    stored        TEXT NOT NULL,   -- where it is while quarantined
    state         TEXT NOT NULL,   -- quarantined | restored | purged
    changed       TEXT NOT NULL,   -- UTC, when the state last changed
    filename      TEXT, size INTEGER, modified_date TEXT, access_date TEXT, birth_date TEXT,
    checksum      TEXT, extension TEXT, directory TEXT, device INTEGER, inode INTEGER, quick TEXT
);
CREATE INDEX IF NOT EXISTS idx_quarantined_batch ON quarantined(batch);
CREATE INDEX IF NOT EXISTS idx_quarantined_original ON quarantined(original);";

/// The file-row columns kept while a file is quarantined, so restoring it needs no re-read
const ROW_COLUMNS: [&str; 11] =
    ["filename", "size", "modified_date", "access_date", "birth_date", "checksum", "extension", "directory", "device", "inode", "quick"];

pub(crate) enum QuarantineCommand<'a> {
    Add { paths: &'a [String], stdin: bool, null: bool, reason: Option<&'a str>, dry_run: bool, yes: bool },
    List,
    Show { batch: &'a str },
    Restore { batch: &'a str, paths: &'a [String], dry_run: bool },
    Purge { batch: Option<&'a str>, older_than: Option<&'a str>, dry_run: bool, yes: bool },
}

fn db_err(e: rusqlite::Error) -> String {
    e.to_string()
}

pub(crate) fn open(db_path: &str, write: bool) -> Result<Connection, String> {
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    let mut conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    if write {
        migrate_schema(&mut conn)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
    }
    Ok(conn)
}

fn has_tables(conn: &Connection) -> Result<bool, String> {
    table_exists(conn, "quarantined").map_err(db_err)
}

pub(crate) fn ask(question: &str, yes: bool) -> Result<(), String> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err("not running interactively — pass --yes to confirm".to_string());
    }
    eprint!("{} [y/N] ", question);
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
    if matches!(answer.trim(), "y" | "Y" | "yes") { Ok(()) } else { Err("cancelled".to_string()) }
}

/// A path as given, made absolute with its folders resolved but not the file itself — so a
/// symlink is the link, not what it points to
pub(crate) fn absolute_file(p: &str) -> Result<PathBuf, String> {
    let path = Path::new(p);
    let name = path.file_name().ok_or_else(|| format!("not a file: {}", p))?;
    let parent = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let dir = fs::canonicalize(parent).map_err(|e| format!("{}: {}", p, e))?;
    Ok(dir.join(name))
}

/// A file to quarantine, with its row from the index
pub(crate) struct Planned {
    root: String,
    table: String,
    also: Vec<String>, // other tables holding it: indexed paths that overlap its own
    pub(crate) original: PathBuf,
    stored: PathBuf,
    row: Vec<rusqlite::types::Value>, // ROW_COLUMNS, in order
    pub(crate) size: u64,
    pub(crate) checksum: Option<String>,
}

/// The indexed path a file belongs to (the deepest one that contains it)
fn root_of<'a>(roots: &'a [String], file: &Path) -> Option<&'a String> {
    roots.iter().filter(|r| file.starts_with(r.as_str())).max_by_key(|r| r.len())
}

fn select_row(conn: &Connection, table: &str) -> Result<String, String> {
    let cols: Vec<String> = ROW_COLUMNS
        .iter()
        .map(|c| Ok(if has_column(conn, table, c).map_err(db_err)? { c.to_string() } else { "NULL".to_string() }))
        .collect::<Result<_, String>>()?;
    Ok(format!("SELECT {} FROM {table} WHERE path = ?1", cols.join(", ")))
}

pub(crate) fn new_batch(conn: &Connection) -> Result<String, String> {
    let base = chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string();
    let mut name = base.clone();
    let mut n = 2;
    while conn
        .query_row("SELECT 1 FROM quarantine_batches WHERE batch = ?1", [&name], |_| Ok(()))
        .optional()
        .map_err(db_err)?
        .is_some()
    {
        name = format!("{}-{}", base, n);
        n += 1;
    }
    Ok(name)
}

fn read_paths(paths: &[String], stdin: bool, null: bool) -> Result<Vec<String>, String> {
    let mut all = paths.to_vec();
    if stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(|e| e.to_string())?;
        let sep = if null { '\0' } else { '\n' };
        all.extend(text.split(sep).map(|s| s.trim_end_matches('\r')).filter(|s| !s.is_empty()).map(str::to_string));
    }
    if all.is_empty() {
        return Err("no files given — name them, or pass --stdin (e.g. from `search … --paths`)".to_string());
    }
    Ok(all)
}

/// Check each file against the index; the ones that can go, and why the others can't
pub(crate) fn plan(conn: &Connection, batch: &str, given: &[String]) -> Result<(Vec<Planned>, Vec<String>), String> {
    let roots: Vec<String> = load_roots(conn).map_err(db_err)?.into_values().map(|r| r.root).collect();
    let mut planned = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for p in given {
        let file = match absolute_file(p) {
            Ok(f) => f,
            Err(e) => {
                skipped.push(e);
                continue;
            }
        };
        if !seen.insert(file.clone()) {
            continue;
        }
        let show = file.display().to_string();
        let meta = match fs::symlink_metadata(&file) {
            Ok(m) if m.file_type().is_file() => m,
            Ok(_) => {
                skipped.push(format!("{} — not a regular file", show));
                continue;
            }
            Err(e) => {
                skipped.push(format!("{} — {}", show, e));
                continue;
            }
        };
        let Some(root) = root_of(&roots, &file) else {
            skipped.push(format!("{} — not in an indexed path of this database", show));
            continue;
        };
        if file.components().any(|c| c.as_os_str() == FOLDER) {
            skipped.push(format!("{} — already in quarantine", show));
            continue;
        }
        let table = table_name_for(root);
        let row: Option<Vec<rusqlite::types::Value>> = conn
            .query_row(&select_row(conn, &table)?, [show.as_str()], |r| (0..ROW_COLUMNS.len()).map(|i| r.get(i)).collect())
            .optional()
            .map_err(db_err)?;
        let Some(row) = row else {
            skipped.push(format!("{} — not in the index (run update first)", show));
            continue;
        };
        // The file must be the one the index describes
        let (size, modified) = match (&row[1], &row[2]) {
            (rusqlite::types::Value::Integer(s), rusqlite::types::Value::Text(m)) => (*s as u64, m.clone()),
            _ => (u64::MAX, String::new()),
        };
        let now_modified = meta.modified().ok().map(fmt_time).unwrap_or_default();
        if meta.len() != size || now_modified != modified {
            skipped.push(format!("{} — changed since it was indexed (run update first)", show));
            continue;
        }
        let checksum = match &row[5] {
            rusqlite::types::Value::Text(c) if !c.is_empty() => Some(c.clone()),
            _ => None,
        };
        let inside = file.strip_prefix(root).unwrap_or(&file);
        let stored = Path::new(root).join(FOLDER).join(batch).join(inside);
        let also = roots.iter().filter(|r| *r != root && file.starts_with(r.as_str())).map(|r| table_name_for(r)).collect();
        planned.push(Planned { root: root.clone(), table, also, original: file, stored, row, size, checksum });
    }
    Ok((planned, skipped))
}

fn write_manifest(dir: &Path, batch: &str, db_path: &str, files: &[&Planned]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut f = fs::File::create(dir.join(MANIFEST))?;
    writeln!(f, "# diskunder quarantine {} — {}", batch, chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))?;
    writeln!(f, "# restore: diskunder quarantine restore {} --db {}", batch, db_path)?;
    writeln!(f, "# or move each file back by hand: stored → original")?;
    writeln!(f, "original\tstored\tsize\tchecksum")?;
    for p in files {
        writeln!(f, "{}\t{}\t{}\t{}", p.original.display(), p.stored.display(), p.size, p.checksum.as_deref().unwrap_or(""))?;
    }
    f.sync_all()
}

fn add(
    db_path: &str,
    settings: &Settings,
    given: &[String],
    reason: Option<&str>,
    dry_run: bool,
    yes: bool,
    runlog: &mut RunLog,
) -> Result<(), String> {
    // A dry run changes nothing, so it doesn't migrate: plan against the database as it is
    let conn = open(db_path, !dry_run)?;
    let batch = if dry_run { chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string() } else { new_batch(&conn)? };
    let (planned, skipped) = plan(&conn, &batch, given)?;
    for s in &skipped {
        eprintln!("skip: {}", s);
    }
    let bytes: u64 = planned.iter().map(|p| p.size).sum();
    let mut by_root: BTreeMap<&str, Vec<&Planned>> = BTreeMap::new();
    for p in &planned {
        by_root.entry(&p.root).or_default().push(p);
    }
    let summary = format!("{} · {}", plural(planned.len(), "file"), fmt_bytes(bytes));
    if planned.is_empty() {
        return Err(if skipped.is_empty() { "nothing to quarantine".into() } else { "nothing to quarantine — every file was skipped".into() });
    }
    if dry_run {
        for (root, files) in &by_root {
            println!("{} — {} into {}/{}/{}/", root, plural(files.len(), "file"), root, FOLDER, batch);
            for p in files {
                println!("  {}", p.original.display());
            }
        }
        println!("\nWould quarantine {} (dry run — nothing was moved)", summary);
        return Ok(());
    }
    let ask_first = settings.yes("deletion.confirm_quarantine");
    ask(&format!("Quarantine {}? They can be restored with `quarantine restore {}`.", summary, batch), yes || !ask_first)?;

    let _lock = lock_db(db_path, &format!("diskunder quarantine add ({})", batch))?;
    let result = crate::maintenance::logged(runlog, || {
        let done = move_planned(&conn, db_path, &batch, reason, &planned.iter().collect::<Vec<_>>())?;
        Ok(done.message(&batch, skipped.len()))
    })?;
    println!("{}", result);
    Ok(())
}

/// What `move_planned` did
pub(crate) struct Moved {
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    pub(crate) failed: usize,
}

impl Moved {
    pub(crate) fn message(&self, batch: &str, skipped: usize) -> String {
        let mut msg = format!("quarantined {} ({}) as {}", plural(self.files, "file"), fmt_bytes(self.bytes), batch);
        if self.failed > 0 {
            msg.push_str(&format!("; {} couldn't be moved", plural(self.failed, "file")));
        }
        if skipped > 0 {
            msg.push_str(&format!("; {} skipped", plural(skipped, "file")));
        }
        msg
    }
}

/// Record the batch, then move each planned file into it — the caller holds the database lock
pub(crate) fn move_planned(conn: &Connection, db_path: &str, batch: &str, reason: Option<&str>, planned: &[&Planned]) -> Result<Moved, String> {
    let mut by_root: BTreeMap<&str, Vec<&Planned>> = BTreeMap::new();
    for p in planned {
        by_root.entry(&p.root).or_default().push(*p);
    }
    conn.execute(
        "INSERT INTO quarantine_batches (batch, created, reason, command) VALUES (?1, datetime('now'), ?2, ?3)",
        params![batch, reason, std::env::args().skip(1).collect::<Vec<_>>().join(" ")],
    )
    .map_err(db_err)?;
    let mut done = Moved { files: 0, bytes: 0, failed: 0 };
    let mut affected = Vec::new();
    for (root, files) in &by_root {
        // The manifest goes first, so even an interrupted batch can be put back by hand
        let dir = Path::new(root).join(FOLDER).join(batch);
        write_manifest(&dir, batch, db_path, files).map_err(|e| format!("can't write the manifest in {}: {}", dir.display(), e))?;
        for p in files {
            let moved_ok = p.stored.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::rename(&p.original, &p.stored));
            if let Err(e) = moved_ok {
                // Never copy-and-delete: a rename that fails (e.g. another filesystem
                // mounted inside the root) leaves the file where it is
                eprintln!("not moved: {} — {}", p.original.display(), e);
                done.failed += 1;
                continue;
            }
            record_quarantined(conn, batch, p).map_err(|e| {
                format!("{} was moved to {} but couldn't be recorded: {}", p.original.display(), p.stored.display(), e)
            })?;
            done.files += 1;
            done.bytes += p.size;
            affected.extend(p.checksum.clone());
        }
    }
    refresh_dup_groups(conn, &affected).map_err(db_err)?;
    Ok(done)
}

/// Move the file's row from its table to `quarantined`
fn record_quarantined(conn: &Connection, batch: &str, p: &Planned) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let mut values: Vec<rusqlite::types::Value> = vec![
        batch.to_string().into(),
        p.root.clone().into(),
        p.original.display().to_string().into(),
        p.stored.display().to_string().into(),
    ];
    values.extend(p.row.iter().cloned());
    tx.execute(
        &format!(
            "INSERT INTO quarantined (batch, root, original, stored, state, changed, {}) VALUES (?1, ?2, ?3, ?4, 'quarantined', datetime('now'), {})",
            ROW_COLUMNS.join(", "),
            (5..5 + ROW_COLUMNS.len()).map(|i| format!("?{}", i)).collect::<Vec<_>>().join(", ")
        ),
        rusqlite::params_from_iter(values.iter()),
    )?;
    for t in std::iter::once(&p.table).chain(&p.also) {
        if table_exists(&tx, t)? {
            tx.execute(&format!("DELETE FROM {t} WHERE path = ?1"), [p.original.display().to_string()])?;
        }
    }
    tx.commit()
}

/// After `copy` became a hard link to `keep`: its rows take the kept file's identity and dates,
/// so it counts as the same copy
pub(crate) fn relinked(conn: &Connection, copy: &Planned, keep: &Planned) -> rusqlite::Result<()> {
    const TAKEN: [&str; 5] = ["modified_date", "access_date", "birth_date", "device", "inode"];
    for t in std::iter::once(&copy.table).chain(&copy.also) {
        if !table_exists(conn, t)? {
            continue;
        }
        let cols: Vec<&str> = TAKEN.iter().copied().filter(|c| has_column(conn, t, c).unwrap_or(false)).collect();
        let mut values: Vec<rusqlite::types::Value> = vec![copy.original.display().to_string().into()];
        for c in &cols {
            values.push(keep.row[ROW_COLUMNS.iter().position(|x| x == c).unwrap()].clone());
        }
        let sets: Vec<String> = cols.iter().enumerate().map(|(i, c)| format!("{c} = ?{}", i + 2)).collect();
        conn.execute(&format!("UPDATE {t} SET {} WHERE path = ?1", sets.join(", ")), rusqlite::params_from_iter(values.iter()))?;
    }
    Ok(())
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    format!("{} {}{}", fmt_count(n as i64), word, if n == 1 { "" } else { "s" })
}

fn list(db_path: &str) -> Result<(), String> {
    let conn = open(db_path, false)?;
    if !has_tables(&conn)? {
        println!("nothing has been quarantined in {}", db_path);
        return Ok(());
    }
    let mut stmt = conn
        .prepare(
            "SELECT b.batch, b.created, COALESCE(b.reason, ''),
                    SUM(q.state = 'quarantined'), COALESCE(SUM(CASE WHEN q.state = 'quarantined' THEN q.size END), 0),
                    SUM(q.state = 'restored'), SUM(q.state = 'purged')
             FROM quarantine_batches b LEFT JOIN quarantined q ON q.batch = b.batch
             GROUP BY b.batch ORDER BY b.batch",
        )
        .map_err(db_err)?;
    let rows: Vec<Vec<String>> = stmt
        .query_map([], |r| {
            let (q, bytes, restored, purged): (i64, i64, i64, i64) =
                (r.get::<_, Option<i64>>(3)?.unwrap_or(0), r.get(4)?, r.get::<_, Option<i64>>(5)?.unwrap_or(0), r.get::<_, Option<i64>>(6)?.unwrap_or(0));
            let mut other = Vec::new();
            if restored > 0 {
                other.push(format!("{} restored", fmt_count(restored)));
            }
            if purged > 0 {
                other.push(format!("{} purged", fmt_count(purged)));
            }
            Ok(vec![
                r.get::<_, String>(0)?,
                fmt_local(&r.get::<_, String>(1)?),
                fmt_count(q),
                fmt_bytes(bytes as u64),
                other.join(", "),
                r.get::<_, String>(2)?,
            ])
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    if rows.is_empty() {
        println!("nothing has been quarantined in {}", db_path);
        return Ok(());
    }
    let mut out = Out::new();
    write_text_table(
        &mut out,
        &["BATCH", "CREATED", "FILES", "SIZE", "ALSO", "REASON"],
        &[Align::Left, Align::Left, Align::Right, Align::Right, Align::Left, Align::Left],
        &rows,
    );
    out.flush();
    Ok(())
}

fn show(db_path: &str, batch: &str) -> Result<(), String> {
    let conn = open(db_path, false)?;
    if !has_tables(&conn)? {
        return Err(format!("no batch {}", batch));
    }
    let mut stmt = conn
        .prepare("SELECT state, size, original FROM quarantined WHERE batch = ?1 ORDER BY original")
        .map_err(db_err)?;
    let rows: Vec<Vec<String>> = stmt
        .query_map([batch], |r| Ok(vec![r.get(0)?, fmt_bytes(r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64), r.get(2)?]))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    if rows.is_empty() {
        return Err(format!("no batch {} — see `diskunder quarantine list`", batch));
    }
    let mut out = Out::new();
    write_text_table(&mut out, &["STATE", "SIZE", "ORIGINAL"], &[Align::Left, Align::Right, Align::Left], &rows);
    out.flush();
    Ok(())
}

/// A quarantined file, as recorded
struct Held {
    id: i64,
    root: String,
    original: String,
    stored: String,
    size: i64,
    checksum: Option<String>,
    row: Vec<rusqlite::types::Value>,
}

fn held(conn: &Connection, filter: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Held>, String> {
    let sql = format!(
        "SELECT q.id, q.root, q.original, q.stored, COALESCE(q.size, 0), q.checksum, {} FROM quarantined q
         JOIN quarantine_batches b ON b.batch = q.batch WHERE q.state = 'quarantined' AND {filter} ORDER BY q.original",
        ROW_COLUMNS.iter().map(|c| format!("q.{c}")).collect::<Vec<_>>().join(", ")
    );
    let mut stmt = conn.prepare(&sql).map_err(db_err)?;
    let rows = stmt
        .query_map(params, |r| {
            Ok(Held {
                id: r.get(0)?,
                root: r.get(1)?,
                original: r.get(2)?,
                stored: r.get(3)?,
                size: r.get(4)?,
                checksum: r.get(5)?,
                row: (6..6 + ROW_COLUMNS.len()).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err);
    rows
}

/// Remove folders left empty under a batch folder — and the batch folder itself once only its
/// manifest is left
fn tidy(batch_dir: &Path) {
    fn prune(dir: &Path) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    prune(&e.path());
                    let _ = fs::remove_dir(e.path()); // only succeeds when empty
                }
            }
        }
    }
    prune(batch_dir);
    let only_manifest = fs::read_dir(batch_dir).is_ok_and(|mut d| d.all(|e| e.is_ok_and(|e| e.file_name() == MANIFEST)));
    if only_manifest {
        let _ = fs::remove_file(batch_dir.join(MANIFEST));
        let _ = fs::remove_dir(batch_dir);
        if let Some(q) = batch_dir.parent() {
            let _ = fs::remove_dir(q); // the quarantine folder, if that was its last batch
        }
    }
}

fn restore(db_path: &str, batch: &str, paths: &[String], dry_run: bool, runlog: &mut RunLog) -> Result<(), String> {
    let conn = open(db_path, !dry_run)?;
    if !has_tables(&conn)? {
        return Err(format!("no batch {}", batch));
    }
    let mut files = held(&conn, "q.batch = ?1", &[&batch])?;
    if !paths.is_empty() {
        let wanted: HashSet<String> =
            paths.iter().map(|p| absolute_file(p).map(|f| f.display().to_string()).unwrap_or_else(|_| p.clone())).collect();
        files.retain(|f| wanted.contains(&f.original));
    }
    if files.is_empty() {
        return Err(format!("nothing in batch {} to restore — see `diskunder quarantine show {}`", batch, batch));
    }
    if dry_run {
        for f in &files {
            println!("  {}", f.original);
        }
        println!("\nWould restore {} (dry run — nothing was moved)", plural(files.len(), "file"));
        return Ok(());
    }
    let _lock = lock_db(db_path, &format!("diskunder quarantine restore {}", batch))?;
    let result = crate::maintenance::logged(runlog, || {
        let (mut done, mut problems, mut affected, mut dirs) = (0usize, Vec::new(), Vec::new(), HashSet::new());
        for f in &files {
            let (original, stored) = (Path::new(&f.original), Path::new(&f.stored));
            if original.exists() || fs::symlink_metadata(original).is_ok() {
                problems.push(format!("{} — something is there now; left in quarantine", f.original));
                continue;
            }
            if !stored.exists() {
                problems.push(format!("{} — missing from quarantine ({})", f.original, f.stored));
                continue;
            }
            if let Err(e) = original.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::rename(stored, original)) {
                problems.push(format!("{} — {}", f.original, e));
                continue;
            }
            put_back(&conn, f).map_err(|e| format!("{} was restored but its row couldn't be: {} (run update)", f.original, e))?;
            affected.extend(f.checksum.clone());
            dirs.insert(Path::new(&f.root).join(FOLDER).join(batch));
            done += 1;
        }
        refresh_dup_groups(&conn, &affected).map_err(db_err)?;
        for d in &dirs {
            tidy(d);
        }
        for p in &problems {
            eprintln!("not restored: {}", p);
        }
        let mut msg = format!("restored {} from {}", plural(done, "file"), batch);
        if !problems.is_empty() {
            msg.push_str(&format!("; {} not restored", plural(problems.len(), "file")));
        }
        Ok(msg)
    })?;
    println!("{}", result);
    Ok(())
}

/// The file's row back in its table (if the index still exists), and its state recorded
fn put_back(conn: &Connection, f: &Held) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let table = table_name_for(&f.root);
    if table_exists(&tx, &table)? {
        let cols: Vec<&str> = ROW_COLUMNS.iter().copied().filter(|c| has_column(&tx, &table, c).unwrap_or(false)).collect();
        let mut values: Vec<rusqlite::types::Value> = vec![f.original.clone().into()];
        for c in &cols {
            let i = ROW_COLUMNS.iter().position(|x| x == c).unwrap();
            values.push(f.row[i].clone());
        }
        tx.execute(
            &format!(
                "INSERT OR REPLACE INTO {table} (path, {}) VALUES (?1, {})",
                cols.join(", "),
                (2..2 + cols.len()).map(|i| format!("?{}", i)).collect::<Vec<_>>().join(", ")
            ),
            rusqlite::params_from_iter(values.iter()),
        )?;
    }
    tx.execute("UPDATE quarantined SET state = 'restored', changed = datetime('now') WHERE id = ?1", [f.id])?;
    tx.commit()
}

fn purge(
    db_path: &str,
    settings: &Settings,
    batch: Option<&str>,
    older_than: Option<&str>,
    dry_run: bool,
    yes: bool,
    runlog: &mut RunLog,
) -> Result<(), String> {
    let conn = open(db_path, !dry_run)?;
    if !has_tables(&conn)? {
        println!("nothing has been quarantined in {}", db_path);
        return Ok(());
    }
    let age = older_than.map(str::to_string).unwrap_or_else(|| format!("{}d", settings.number("deletion.purge_after_days")));
    let cutoff = crate::report::parse_age_arg(&age)?.cutoff;
    let files = match batch {
        Some(b) => held(&conn, "q.batch = ?1 AND b.created <= ?2", &[&b, &cutoff])?,
        None => held(&conn, "b.created <= ?1", &[&cutoff])?,
    };
    if files.is_empty() {
        println!("nothing quarantined longer than {}{}", age, batch.map(|b| format!(" in {}", b)).unwrap_or_default());
        return Ok(());
    }
    let bytes: i64 = files.iter().map(|f| f.size).sum();
    let summary = format!("{} · {}", plural(files.len(), "file"), fmt_bytes(bytes as u64));
    if dry_run {
        for f in &files {
            println!("  {}", f.original);
        }
        println!("\nWould permanently delete {} quarantined longer than {} (dry run — nothing was deleted)", summary, age);
        return Ok(());
    }
    ask(&format!("Permanently delete {} quarantined longer than {}? This can't be undone.", summary, age), yes)?;
    let _lock = lock_db(db_path, "diskunder quarantine purge")?;
    let result = crate::maintenance::logged(runlog, || {
        let (mut done, mut freed, mut problems, mut dirs) = (0usize, 0i64, Vec::new(), HashSet::new());
        for f in &files {
            match fs::remove_file(&f.stored) {
                Ok(()) => {}
                // Already gone: nothing to delete, so it counts as purged
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    problems.push(format!("{} — {}", f.stored, e));
                    continue;
                }
            }
            conn.execute("UPDATE quarantined SET state = 'purged', changed = datetime('now') WHERE id = ?1", [f.id]).map_err(db_err)?;
            if let Some(dir) = Path::new(&f.stored)
                .strip_prefix(Path::new(&f.root).join(FOLDER))
                .ok()
                .and_then(|rest| rest.components().next())
            {
                dirs.insert(Path::new(&f.root).join(FOLDER).join(dir.as_os_str()));
            }
            done += 1;
            freed += f.size;
        }
        for d in &dirs {
            tidy(d);
        }
        for p in &problems {
            eprintln!("not deleted: {}", p);
        }
        let mut msg = format!("purged {}, freeing {}", plural(done, "file"), fmt_bytes(freed as u64));
        if !problems.is_empty() {
            msg.push_str(&format!("; {} couldn't be deleted", plural(problems.len(), "file")));
        }
        Ok(msg)
    })?;
    println!("{}", result);
    Ok(())
}

pub(crate) fn run_quarantine_command(db_path: &str, cmd: QuarantineCommand, settings: &Settings, runlog: &mut RunLog) -> i32 {
    let result = match cmd {
        QuarantineCommand::Add { paths, stdin, null, reason, dry_run, yes } => {
            read_paths(paths, stdin, null).and_then(|given| add(db_path, settings, &given, reason, dry_run, yes, runlog))
        }
        QuarantineCommand::List => list(db_path),
        QuarantineCommand::Show { batch } => show(db_path, batch),
        QuarantineCommand::Restore { batch, paths, dry_run } => restore(db_path, batch, paths, dry_run, runlog),
        QuarantineCommand::Purge { batch, older_than, dry_run, yes } => purge(db_path, settings, batch, older_than, dry_run, yes, runlog),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}
