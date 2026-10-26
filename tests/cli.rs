// End-to-end tests: run the real binary in plain mode against throwaway directories,
// then check its summary output and the SQLite database it wrote.

use rusqlite::Connection;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_diskunder");

/// The binary, kept away from your real settings: a per-user store that never exists (jobs
/// and settings tests pass --config-db, which wins)
fn bin() -> Command {
    let mut c = Command::new(BIN);
    c.env("DISKUNDER_CONFIG_DB", std::env::temp_dir().join(format!("diskunder-test-nostore-{}.db", std::process::id())));
    c
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Temporary directory removed on drop. Canonicalised, because the tool stores
/// canonical paths (on macOS the temp dir sits behind the /var → /private/var symlink).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "diskunder-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(fs::canonicalize(&dir).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Restore permissions changed by tests so cleanup can't fail
        let _ = Command::new("chmod").arg("-R").arg("u+rwx").arg(&self.0).status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    File::create(path).unwrap().write_all(contents.as_bytes()).unwrap();
}

fn set_mtime(path: &Path, t: SystemTime) {
    File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60)
}

/// Parsed final summary line, e.g.
/// `complete: 10 files — 2 already indexed, 8 written, 0 removed, 0 errors, 1 duplicate groups`
#[derive(Debug)]
struct Summary {
    found: usize,
    skipped: usize,
    written: usize,
    removed: usize,
    errors: String, // "0 errors" or "3 errors (2 unreadable, 1 traversal/db)"
    groups: usize,
    wasted: Option<String>, // from the `wasted: …` line, if printed
}

struct Run {
    stderr: String,
    summary: Summary,
}

fn run_in(cwd: &Path, args: &[&str]) -> Run {
    run_expecting(cwd, args, 0)
}

/// For runs that stop on a fatal error, which exit with status 1
fn run_failing(cwd: &Path, args: &[&str]) -> Run {
    run_expecting(cwd, args, 1)
}

fn run_expecting(cwd: &Path, args: &[&str], code: i32) -> Run {
    let out = bin()
        .args(args)
        .arg("--no-tui")
        .current_dir(cwd)
        .output()
        .expect("failed to run diskunder");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "unexpected exit status:\n{}", stderr);
    let summary = parse_summary(&stderr);
    Run { stderr, summary }
}

fn parse_summary(stderr: &str) -> Summary {
    let line = stderr
        .lines()
        .find(|l| l.starts_with("complete: "))
        .unwrap_or_else(|| panic!("no summary line in:\n{}", stderr));
    let num = |s: &str| -> usize { s.trim().split(' ').next().unwrap().parse().unwrap() };

    let rest = line.strip_prefix("complete: ").unwrap();
    let (found, rest) = rest.split_once(" files — ").unwrap();
    let parts: Vec<&str> = rest.split(", ").collect();
    // errors may contain ", " inside its parentheses, so take fixed fields from each end
    let groups = parts.last().unwrap();
    let errors = parts[3..parts.len() - 1].join(", ");
    Summary {
        found: num(found),
        skipped: num(parts[0]),
        written: num(parts[1]),
        removed: num(parts[2]),
        errors,
        groups: num(groups),
        wasted: stderr
            .lines()
            .find_map(|l| l.strip_prefix("wasted: ").map(str::to_owned)),
    }
}

fn table_for(root: &Path) -> String {
    format!("files_{:x}", md5::compute(root.to_string_lossy().as_bytes()))
}

fn count(db: &Path, sql: &str) -> i64 {
    Connection::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn columns(db: &Path, table: &str) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn.prepare(&format!("SELECT name FROM pragma_table_info('{}')", table)).unwrap();
    let cols = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    cols
}

fn tables(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'files_%' ORDER BY name")
        .unwrap();
    let names = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    names
}

/// A directory of `n` small files, all distinct
fn distinct_files(root: &Path, n: usize) {
    for i in 0..n {
        write(&root.join(format!("f{}", i)), &format!("file {}\n", i));
    }
}

/// True when permission bits are not enforced (running as root), so chmod-based tests can't work
fn permissions_ignored(locked: &Path) -> bool {
    fs::read_dir(locked).is_ok()
}

fn chmod(path: &Path, mode: &str) {
    assert!(Command::new("chmod").arg(mode).arg(path).status().unwrap().success());
}

// ─── Resume (index mode) ─────────────────────────────────────────────────────

#[test]
fn reindex_skips_everything_already_indexed() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 50);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();

    let first = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!((first.found, first.skipped, first.written), (50, 0, 50));

    let second = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!((second.found, second.skipped, second.written), (50, 50, 0));
}

#[test]
fn resume_hashes_only_missing_rows() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 60);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    // Simulate a crash partway through by dropping a third of the rows
    let t = table_for(&data);
    Connection::open(&db).unwrap().execute(&format!("DELETE FROM {} WHERE id % 3 = 0", t), []).unwrap();
    let remaining = count(&db, &format!("SELECT COUNT(*) FROM {}", t)) as usize;

    let s = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!(s.skipped, remaining);
    assert_eq!(s.written, 60 - remaining);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {}", t)), 60);
}

// ─── Update mode ─────────────────────────────────────────────────────────────

#[test]
fn update_rehashes_only_changed_files() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 20);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    // Size change; same size but different modified time; brand new file
    write(&data.join("f0"), "a longer replacement\n");
    write(&data.join("f1"), "file X\n");
    set_mtime(&data.join("f1"), days_ago(3));
    write(&data.join("new"), "new\n");

    let s = run_in(tmp.path(), &["update", "data", "--db", db_s]).summary;
    assert_eq!((s.found, s.skipped, s.written, s.removed), (21, 18, 3, 0));

    let again = run_in(tmp.path(), &["update", "data", "--db", db_s]).summary;
    assert_eq!((again.skipped, again.written), (21, 0));
}

#[test]
fn update_catches_changes_with_old_modified_times() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 5);
    for i in 0..5 {
        set_mtime(&data.join(format!("f{}", i)), days_ago(400));
    }
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    // Edited but back-dated, and a copy that keeps its old date (as rsync -a / cp -p would)
    write(&data.join("f0"), "edited, longer\n");
    set_mtime(&data.join("f0"), days_ago(400));
    fs::copy(data.join("f1"), data.join("copy")).unwrap();
    set_mtime(&data.join("copy"), days_ago(400));

    let s = run_in(tmp.path(), &["update", "data", "--db", db_s]).summary;
    assert_eq!(s.written, 2, "both old-dated changes should be picked up");
}

#[test]
fn update_removes_deleted_and_renamed_files() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 10);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    fs::remove_file(data.join("f0")).unwrap();
    fs::remove_file(data.join("f1")).unwrap();
    fs::rename(data.join("f2"), data.join("f2_renamed")).unwrap();

    let s = run_in(tmp.path(), &["update", "data", "--db", db_s]).summary;
    assert_eq!((s.found, s.written, s.removed), (8, 1, 3));

    let t = table_for(&data);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {}", t)), 8);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE path LIKE '%/f2'", t)), 0);
}

#[test]
fn index_never_removes_rows() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 5);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    fs::remove_file(data.join("f0")).unwrap();
    let s = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!(s.removed, 0);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {}", table_for(&data))), 5);
}

#[test]
fn update_keeps_rows_under_unreadable_directory() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data.join("open"), 5);
    distinct_files(&data.join("locked"), 3);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    let locked = data.join("locked");
    chmod(&locked, "000");
    if permissions_ignored(&locked) {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }

    let run = run_in(tmp.path(), &["update", "data", "--db", db_s]);
    assert_eq!(run.summary.removed, 0);
    assert!(run.summary.errors.contains("1 traversal/db"), "{}", run.summary.errors);
    assert!(run.stderr.contains("warn: "), "traversal warning should be printed in plain mode");
    let t = table_for(&data);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE path LIKE '%/locked/%'", t)), 3);
}

#[test]
fn update_removes_nothing_when_walk_finds_no_files() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 5);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    // Looks like an unmounted share: the directory exists but is empty
    fs::rename(&data, tmp.path().join("moved")).unwrap();
    fs::create_dir(&data).unwrap();

    let run = run_in(tmp.path(), &["update", "data", "--db", db_s]);
    assert_eq!(run.summary.removed, 0);
    assert!(run.stderr.contains("is the share mounted?"), "{}", run.stderr);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {}", table_for(&data))), 5);
}

#[test]
fn update_on_fresh_database_creates_indexes() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 3);
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["update", "data", "--db", db.to_str().unwrap()]);

    let t = table_for(&data);
    let indexes = count(
        &db,
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_{}_%'", t),
    );
    assert_eq!(indexes, 8, "path index plus the seven secondary indexes");
}

// ─── Path normalisation and migration ────────────────────────────────────────

#[test]
fn every_spelling_of_a_path_shares_one_table() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 4);
    std::os::unix::fs::symlink(&data, tmp.path().join("link")).unwrap();
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();

    run_in(tmp.path(), &["index", "data", "--db", db_s]);
    let absolute_slash = format!("{}/", data.display());
    for spelling in ["data/", "./data", "data//", "link", absolute_slash.as_str()] {
        let s = run_in(tmp.path(), &["update", spelling, "--db", db_s]).summary;
        assert_eq!((s.skipped, s.written), (4, 0), "spelling {:?}", spelling);
    }
    assert_eq!(tables(&db), vec![table_for(&data)]);

    let sample: String = Connection::open(&db)
        .unwrap()
        .query_row(&format!("SELECT path FROM {} LIMIT 1", table_for(&data)), [], |r| r.get(0))
        .unwrap();
    assert!(sample.starts_with(&data.to_string_lossy().into_owned()), "stored paths are absolute: {}", sample);
}

/// Create a table as v3 would have: named after the path as typed, relative paths,
/// a `change_date` column and no device/inode columns
fn create_v3_table(db: &Path, typed: &str, files: &[(&str, &str)]) -> String {
    let t = format!("files_{:x}", md5::compute(typed.as_bytes()));
    let conn = Connection::open(db).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE {t} (
            id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE, filename TEXT NOT NULL,
            size INTEGER NOT NULL, modified_date TEXT, access_date TEXT, change_date TEXT,
            checksum TEXT, extension TEXT, directory TEXT, created_at TEXT DEFAULT CURRENT_TIMESTAMP);
         CREATE INDEX idx_{t}_path ON {t}(path);
         CREATE INDEX idx_{t}_size ON {t}(size);",
        t = t
    ))
    .unwrap();
    for (path, checksum) in files {
        let meta = fs::metadata(db.parent().unwrap().join(path)).unwrap();
        let mtime: chrono::DateTime<chrono::Utc> = meta.modified().unwrap().into();
        conn.execute(
            &format!(
                "INSERT INTO {} (path, filename, size, modified_date, change_date, checksum, directory)
                 VALUES (?1, ?2, ?3, ?4, '2020-01-01 00:00:00', ?5, ?6)",
                t
            ),
            rusqlite::params![
                path,
                Path::new(path).file_name().unwrap().to_string_lossy(),
                meta.len(),
                mtime.format("%Y-%m-%d %H:%M:%S").to_string(),
                checksum,
                Path::new(path).parent().unwrap().to_string_lossy(),
            ],
        )
        .unwrap();
    }
    t
}

#[test]
fn v3_table_is_migrated_without_rehashing() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a"), "aaa\n");
    write(&data.join("sub/b"), "bbb\n");
    let db = tmp.path().join("t.db");
    let legacy = create_v3_table(&db, "data/", &[("data/a", "sum-a"), ("data/sub/b", "sum-b")]);

    let run = run_in(tmp.path(), &["update", "data/", "--db", db.to_str().unwrap()]);
    assert!(run.stderr.contains("migrated 2 rows"), "{}", run.stderr);
    assert_eq!((run.summary.skipped, run.summary.written), (2, 0), "nothing re-hashed");

    let t = table_for(&data);
    assert_eq!(tables(&db), vec![t.clone()], "legacy table {} renamed", legacy);
    let cols = columns(&db, &t);
    assert!(cols.contains(&"birth_date".to_string()) && !cols.contains(&"change_date".to_string()));
    assert!(cols.contains(&"device".to_string()) && cols.contains(&"inode".to_string()));

    let (path, dir, birth, checksum): (String, String, String, String) = Connection::open(&db)
        .unwrap()
        .query_row(
            &format!("SELECT path, directory, birth_date, checksum FROM {} WHERE filename = 'b'", t),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(path, data.join("sub/b").to_string_lossy());
    assert_eq!(dir, data.join("sub").to_string_lossy());
    assert_eq!(birth, "2020-01-01 00:00:00", "renamed column keeps its values");
    assert_eq!(checksum, "sum-b", "row was migrated, not re-hashed");

    let old_indexes = count(
        &db,
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_{}_%'", legacy),
    );
    assert_eq!(old_indexes, 0, "indexes named after the old table are dropped");
}

#[test]
fn failed_migration_stops_the_run_and_leaves_the_table() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a"), "aaa\n");
    let db = tmp.path().join("t.db");
    let legacy = create_v3_table(&db, "./data", &[("data/a", "sum-a")]);
    // A row that can't be mapped onto the root
    Connection::open(&db)
        .unwrap()
        .execute(&format!("UPDATE {} SET path = 'elsewhere/x'", legacy), [])
        .unwrap();

    let run = run_failing(tmp.path(), &["index", "./data", "--db", db.to_str().unwrap()]);
    assert!(run.stderr.contains("could not migrate"), "{}", run.stderr);
    assert_eq!(run.summary.written, 0);
    assert_eq!(tables(&db), vec![legacy.clone()]);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE path = 'elsewhere/x'", legacy)), 1);
}

// ─── Error counting ──────────────────────────────────────────────────────────

#[test]
fn errors_are_counted_by_kind() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data.join("ok"), 1500);
    write(&data.join("ok/zz_boom"), "boom\n");
    write(&data.join("unreadable"), "secret\n");
    distinct_files(&data.join("locked"), 2);
    chmod(&data.join("unreadable"), "000");
    chmod(&data.join("locked"), "000");
    if permissions_ignored(&data.join("locked")) {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }

    // Pre-create the table with a trigger that rejects one path, so exactly one batch fails
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    let t = table_for(&data);
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(&format!(
        "DELETE FROM {t};
         CREATE TRIGGER fail BEFORE INSERT ON {t} WHEN NEW.path LIKE '%boom%'
         BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
        t = t
    ))
    .unwrap();
    drop(conn);

    let run = run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    let s = &run.summary;
    assert_eq!(s.found, 1502, "1500 + boom + unreadable; locked contents are never seen");

    // Walk order isn't guaranteed, so work out which batch held the trigger file
    let not_written = 1502 - s.written;
    assert!(not_written >= 1 && not_written <= 1000, "one batch lost: {}", not_written);
    // The unreadable file is only read — and fails — if its row was written: it shares its
    // size with the f* files, so it might be a copy
    let unreadable_recorded = count(&db, &format!("SELECT COUNT(*) FROM {} WHERE path LIKE '%/unreadable'", t)) == 1;
    let expected = if unreadable_recorded {
        format!("{} errors (1 unreadable, {} not written, 1 traversal/db)", not_written + 2, not_written)
    } else {
        format!("{} errors ({} not written, 1 traversal/db)", not_written + 1, not_written)
    };
    assert_eq!(s.errors, expected);

    // Progress never passes the queued total
    for line in run.stderr.lines().filter(|l| l.contains(" processed — ")) {
        let (done, total) = line.trim().split(' ').next().unwrap().split_once('/').unwrap();
        assert!(done.parse::<usize>().unwrap() <= total.parse::<usize>().unwrap(), "{}", line);
    }
}

// ─── Duplicates ──────────────────────────────────────────────────────────────

#[test]
fn duplicate_totals_cover_all_groups_and_ignore_empty_files() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    // 15 groups of 3 identical 6-byte files → 15 groups, 15 × 6 × 2 = 180 B wasted
    for g in 0..15 {
        for c in 0..3 {
            write(&data.join(format!("g{}_{}", g, c)), &format!("dup{:02}\n", g));
        }
    }
    for i in 0..20 {
        write(&data.join(format!("empty{}", i)), "");
    }
    let db = tmp.path().join("t.db");
    let s = run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]).summary;
    assert_eq!(s.groups, 15, "all groups counted, not just the top 10; empty files excluded");
    assert_eq!(s.wasted.as_deref(), Some("180 B"));
}

#[test]
fn duplicates_include_files_skipped_this_run() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    for g in 0..5 {
        for c in 0..2 {
            write(&data.join(format!("g{}_{}", g, c)), &format!("dup{}\n", g));
        }
    }
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    let reindex = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!((reindex.written, reindex.groups), (0, 5));

    // Deleted from disk: no longer a duplicate, even though index mode keeps the row
    fs::remove_file(data.join("g0_1")).unwrap();
    let update = run_in(tmp.path(), &["update", "data", "--db", db_s]).summary;
    assert_eq!(update.groups, 4);
}

#[test]
fn hard_links_are_not_wasted_space() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a"), &"a".repeat(100));
    fs::hard_link(data.join("a"), data.join("a_link")).unwrap();
    write(&data.join("a_copy"), &"a".repeat(100));
    write(&data.join("c"), &"c".repeat(200));
    fs::hard_link(data.join("c"), data.join("c_link")).unwrap();

    let db = tmp.path().join("t.db");
    let s = run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]).summary;
    // a + a_link are one copy, a_copy is the second; c + c_link is a single copy
    assert_eq!(s.groups, 1);
    assert_eq!(s.wasted.as_deref(), Some("100 B"));
}

#[test]
fn update_backfills_device_and_inode_without_rehashing() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("c"), &"c".repeat(200));
    fs::hard_link(data.join("c"), data.join("c_link")).unwrap();
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    // Rows from before device/inode were recorded
    let t = table_for(&data);
    Connection::open(&db).unwrap().execute(&format!("UPDATE {} SET device = NULL, inode = NULL", t), []).unwrap();

    let before = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!(before.groups, 1, "without ids, hard links fall back to counting as copies");

    let run = run_in(tmp.path(), &["update", "data", "--db", db_s]);
    assert!(run.stderr.contains("recorded device/inode for 2 existing rows"), "{}", run.stderr);
    assert_eq!((run.summary.written, run.summary.groups), (0, 0));
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE inode IS NULL", t)), 0);
}

// ─── Misc ────────────────────────────────────────────────────────────────────

#[test]
fn missing_path_fails_before_touching_the_database() {
    let tmp = TempDir::new();
    let db = tmp.path().join("t.db");
    let out = bin()
        .args(["index", "does-not-exist", "--db", db.to_str().unwrap(), "--no-tui"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("path not found"));
    assert!(!db.exists());
}

#[test]
fn global_flags_work_before_or_after_the_subcommand() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 3);
    let (db1, db2) = (tmp.path().join("before.db"), tmp.path().join("after.db"));

    let out = bin()
        .args(["--no-tui", "--db", db1.to_str().unwrap(), "--threads", "2", "index", "data"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(parse_summary(&String::from_utf8_lossy(&out.stderr)).written, 3);

    let s = run_in(tmp.path(), &["index", "data", "--threads", "2", "--db", db2.to_str().unwrap()]).summary;
    assert_eq!(s.written, 3);
    assert!(!tmp.path().join("fileindex.db").exists(), "--db honoured in both positions");
}

#[test]
fn default_database_is_fileindex_db_in_the_working_directory() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    run_in(tmp.path(), &["index", "data"]);
    assert!(tmp.path().join("fileindex.db").exists());
}

#[test]
fn bad_arguments_are_rejected() {
    for args in [&["index"][..], &["frobnicate", "x"], &["index", "x", "--threads", "lots"]] {
        let out = bin().args(args).output().unwrap();
        assert!(!out.status.success(), "{:?} should fail", args);
    }
    let help = bin().arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&help.stdout);
    for word in ["index", "update", "--db", "--threads", "--no-tui"] {
        assert!(text.contains(word), "--help mentions {}:\n{}", word, text);
    }
}

// ─── Database maintenance (`db` subcommand, roots table) ─────────────────────

struct DbRun {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `diskunder db …` with stdin closed (i.e. not interactive)
fn db_cmd(cwd: &Path, args: &[&str]) -> DbRun {
    let out = bin()
        .arg("db")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    DbRun {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn every_run_is_recorded_in_roots() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 3);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data/", "--db", db_s]);

    let t = table_for(&data);
    let conn = Connection::open(&db).unwrap();
    let (root, mode, finished): (String, String, Option<String>) = conn
        .query_row(
            "SELECT root, last_mode, last_finished FROM roots WHERE table_name = ?1",
            [&t],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(root, data.to_string_lossy(), "canonical path recorded, not the spelling typed");
    assert_eq!(mode, "index");
    assert!(finished.is_some());

    conn.execute("UPDATE roots SET first_run = '2020-01-01 00:00:00'", []).unwrap();
    run_in(tmp.path(), &["update", "data", "--db", db_s]);
    let (first, mode): (String, String) = conn
        .query_row("SELECT first_run, last_mode FROM roots WHERE table_name = ?1", [&t], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((first.as_str(), mode.as_str()), ("2020-01-01 00:00:00", "update"));
    assert_eq!(count(&db, "SELECT COUNT(*) FROM roots"), 1);
}

#[test]
fn db_info_lists_each_path_with_duplicates() {
    let tmp = TempDir::new();
    let (photos, docs) = (tmp.path().join("photos"), tmp.path().join("docs"));
    write(&photos.join("a"), &"a".repeat(300));
    write(&photos.join("a_copy"), &"a".repeat(300));
    write(&photos.join("b"), &"b".repeat(500));
    fs::hard_link(photos.join("b"), photos.join("b_link")).unwrap();
    distinct_files(&docs, 1200);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "photos", "--db", db_s]);
    run_in(tmp.path(), &["index", "docs", "--db", db_s]);

    let info = db_cmd(tmp.path(), &["info", "--db", db_s]);
    assert_eq!(info.code, 0, "{}", info.stderr);
    let out = &info.stdout;
    let (p, d) = (out.find(&*photos.to_string_lossy()).unwrap(), out.find(&*docs.to_string_lossy()).unwrap());
    assert!(d < p, "paths listed alphabetically");
    assert!(out.contains("files       4 (1.6 KB)"), "{}", out);
    assert!(out.contains("1 group, 300 B wasted"), "hard link not counted:\n{}", out);
    assert!(out.contains("files       1,200 ("), "{}", out);
    assert!(out.contains("last run    index at "), "{}", out);
}

#[test]
fn db_info_handles_tables_with_no_recorded_path() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a"), "aaa\n");
    let db = tmp.path().join("t.db");
    let legacy = create_v3_table(&db, "data/", &[("data/a", "sum-a")]);

    let info = db_cmd(tmp.path(), &["info", "--db", db.to_str().unwrap()]);
    assert_eq!(info.code, 0, "{}", info.stderr);
    assert!(info.stdout.contains("(no recorded path)"), "{}", info.stdout);
    assert!(info.stdout.contains(&format!("db drop --table {}", legacy)), "{}", info.stdout);

    let check = db_cmd(tmp.path(), &["check", "--db", db.to_str().unwrap()]);
    assert_eq!(check.code, 0);
    assert!(check.stdout.contains("integrity: ok"));
    assert!(check.stdout.contains(&legacy), "{}", check.stdout);
}

#[test]
fn db_info_flags_runs_that_did_not_finish() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    Connection::open(&db).unwrap().execute("UPDATE roots SET last_finished = NULL", []).unwrap();

    let info = db_cmd(tmp.path(), &["info", "--db", db.to_str().unwrap()]);
    assert!(info.stdout.contains("did not finish"), "{}", info.stdout);
}

#[test]
fn db_drop_needs_confirmation_unless_yes() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 3);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);
    let t = table_for(&data);

    let refused = db_cmd(tmp.path(), &["drop", "data", "--db", db_s]);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("--yes"), "{}", refused.stderr);
    assert_eq!(tables(&db), vec![t.clone()]);

    let dropped = db_cmd(tmp.path(), &["drop", "data", "--yes", "--db", db_s]);
    assert_eq!(dropped.code, 0, "{}", dropped.stderr);
    assert!(dropped.stdout.contains("(3 files)"), "{}", dropped.stdout);
    assert!(tables(&db).is_empty());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM roots"), 0);
}

#[test]
fn db_drop_finds_paths_that_no_longer_exist_and_tables_by_name() {
    let tmp = TempDir::new();
    let (gone, legacy_dir) = (tmp.path().join("gone"), tmp.path().join("old"));
    distinct_files(&gone, 2);
    write(&legacy_dir.join("a"), "aaa\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "gone", "--db", db_s]);
    let legacy = create_v3_table(&db, "old/", &[("old/a", "sum-a")]);
    fs::remove_dir_all(&gone).unwrap();

    let by_path = db_cmd(tmp.path(), &["drop", "gone/", "--yes", "--db", db_s]);
    assert_eq!(by_path.code, 0, "{}", by_path.stderr);
    let by_name = db_cmd(tmp.path(), &["drop", "--table", &legacy, "--yes", "--db", db_s]);
    assert_eq!(by_name.code, 0, "{}", by_name.stderr);
    assert!(tables(&db).is_empty());

    let unknown = db_cmd(tmp.path(), &["drop", "nowhere", "--yes", "--db", db_s]);
    assert_eq!(unknown.code, 1);
    assert!(unknown.stderr.contains("no index for"), "{}", unknown.stderr);
    let not_a_table = db_cmd(tmp.path(), &["drop", "--table", "roots", "--yes", "--db", db_s]);
    assert_eq!(not_a_table.code, 1, "only files_* tables can be dropped");
}

#[test]
fn db_vacuum_shrinks_the_file_after_a_drop() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("big"), 3000);
    distinct_files(&tmp.path().join("small"), 2);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "big", "--db", db_s]);
    run_in(tmp.path(), &["index", "small", "--db", db_s]);
    db_cmd(tmp.path(), &["drop", "big", "--yes", "--db", db_s]);

    let size = || fs::metadata(&db).unwrap().len() + fs::metadata(format!("{}-wal", db_s)).map(|m| m.len()).unwrap_or(0);
    let before = size();
    let vacuum = db_cmd(tmp.path(), &["vacuum", "--db", db_s]);
    assert_eq!(vacuum.code, 0, "{}", vacuum.stderr);
    assert!(size() < before / 4, "{} → {}", before, size());
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {}", table_for(&tmp.path().join("small")))), 2);
}

#[test]
fn db_commands_never_create_a_database() {
    let tmp = TempDir::new();
    for action in ["info", "vacuum", "check"] {
        let r = db_cmd(tmp.path(), &[action, "--db", "missing.db"]);
        assert_eq!(r.code, 1, "{}", action);
        assert!(r.stderr.contains("no database at"), "{}", r.stderr);
    }
    assert!(!tmp.path().join("missing.db").exists());
}

// ─── Schema versioning (v5) ──────────────────────────────────────────────────

fn user_version(db: &Path) -> i64 {
    count(db, "PRAGMA user_version")
}

#[test]
fn extensions_are_stored_lowercase_and_older_databases_migrated() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a.JPG"), "a\n");
    write(&data.join("b.Mov"), "b\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);
    assert_eq!(user_version(&db), 5);
    let t = table_for(&data);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE extension IN ('jpg', 'mov')", t)), 2);

    // Turn it back into a v4 database: mixed-case extensions, schema version 0
    let conn = Connection::open(&db).unwrap();
    conn.execute(&format!("UPDATE {} SET extension = 'JPG' WHERE extension = 'jpg'", t), []).unwrap();
    conn.pragma_update(None, "user_version", 0).unwrap();
    drop(conn);

    write(&data.join("c.PNG"), "c\n");
    let run = run_in(tmp.path(), &["update", "data", "--db", db_s]);
    assert!(run.stderr.contains("schema 1: stored 1 file extensions in lowercase"), "{}", run.stderr);
    assert_eq!(user_version(&db), 5);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE extension != lower(extension)", t)), 0);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {} WHERE extension = 'png'", t)), 1);

    let again = run_in(tmp.path(), &["update", "data", "--db", db_s]);
    assert!(!again.stderr.contains("schema 1"), "migration runs once:\n{}", again.stderr);
}

#[test]
fn database_from_a_newer_version_is_not_written() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    let db = tmp.path().join("t.db");
    Connection::open(&db).unwrap().pragma_update(None, "user_version", 99).unwrap();

    let run = run_failing(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    assert!(run.stderr.contains("newer than this diskunder supports"), "{}", run.stderr);
    assert_eq!(run.summary.written, 0);
    assert!(tables(&db).is_empty());
    assert_eq!(user_version(&db), 99);
}

// ─── Run log (v5) ────────────────────────────────────────────────────────────

/// Lines of the run log next to `db`, as (run id, kind, text)
fn log_lines(db: &Path) -> Vec<(String, String, String)> {
    let path = format!("{}.log", db.display());
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            // 2026-10-23T02:00:01+01:00 run=7f3a2c start  text…
            let mut parts = l.splitn(3, ' ');
            let (_ts, id, rest) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
            let (kind, text) = rest.split_once(' ').unwrap();
            (id.trim_start_matches("run=").to_string(), kind.to_string(), text.trim_start().to_string())
        })
        .collect()
}

#[test]
fn index_and_update_are_logged_with_a_summary() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("my data"), 3);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "my data", "--db", db_s]);
    run_in(tmp.path(), &["update", "my data", "--db", db_s]);

    let lines = log_lines(&db);
    let kinds: Vec<&str> = lines.iter().map(|(_, k, _)| k.as_str()).collect();
    assert_eq!(kinds, ["start", "end", "start", "end"], "{:?}", lines);
    assert_eq!(lines[0].0, lines[1].0, "start and end share a run id");
    assert_ne!(lines[0].0, lines[2].0, "each run has its own id");
    assert!(lines[0].2.starts_with(&format!("index 'my data' --db {} --no-tui  (user=", db_s)), "{}", lines[0].2);
    assert!(lines[0].2.contains("version=5."), "{}", lines[0].2);
    assert!(lines[1].2.starts_with("completed in "), "{}", lines[1].2);
    assert!(lines[1].2.contains("— 3 found, 0 already indexed, 3 written, 0 removed, 0 errors, 0 duplicate groups"), "{}", lines[1].2);
    assert!(lines[3].2.contains("3 unchanged, 0 written"), "{}", lines[3].2);
}

#[test]
fn run_errors_are_logged_and_capped_but_file_errors_are_not() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    for i in 0..60 {
        distinct_files(&data.join(format!("locked{:02}", i)), 1);
        chmod(&data.join(format!("locked{:02}", i)), "000");
    }
    write(&data.join("unreadable"), "secret\n");
    chmod(&data.join("unreadable"), "000");
    if permissions_ignored(&data.join("locked00")) {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);

    let lines = log_lines(&db);
    let errors: Vec<&String> = lines.iter().filter(|(_, k, _)| k == "error").map(|(_, _, t)| t).collect();
    assert_eq!(errors.len(), 51, "50 errors, then a count of the rest");
    assert!(errors.iter().take(50).all(|t| t.contains("locked")), "{:?}", errors);
    assert_eq!(errors[50], "… and 10 more errors (not logged)");
    assert!(!errors.iter().any(|t| t.contains("unreadable")), "per-file errors stay out of the log");
}

#[test]
fn failed_runs_are_logged_with_their_exit_code() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 1);
    let db = tmp.path().join("t.db");
    Connection::open(&db).unwrap().pragma_update(None, "user_version", 99).unwrap();
    run_failing(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);

    let lines = log_lines(&db);
    let end = &lines.last().unwrap().2;
    assert!(end.starts_with("failed in ") && end.contains("(exit 1) — db — could not update database schema"), "{}", end);
    assert!(lines.iter().any(|(_, k, t)| k == "error" && t.contains("newer than this diskunder")));
}

#[test]
fn log_location_can_be_changed_or_turned_off() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 1);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();

    run_in(tmp.path(), &["index", "data", "--db", db_s, "--no-log"]);
    assert!(!tmp.path().join("t.db.log").exists());

    let custom = tmp.path().join("logs-here.txt");
    run_in(tmp.path(), &["update", "data", "--db", db_s, "--log", custom.to_str().unwrap()]);
    assert!(!tmp.path().join("t.db.log").exists());
    assert_eq!(fs::read_to_string(&custom).unwrap().lines().count(), 2);
}

#[test]
fn an_unwritable_log_never_fails_the_run() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    let db = tmp.path().join("t.db");
    let run = run_in(
        tmp.path(),
        &["index", "data", "--db", db.to_str().unwrap(), "--log", "/nonexistent-dir/x.log"],
    );
    assert_eq!(run.summary.written, 2);
    assert!(run.stderr.contains("warning: could not write run log /nonexistent-dir/x.log"), "{}", run.stderr);
}

#[test]
fn large_logs_are_rotated() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 1);
    let db = tmp.path().join("t.db");
    let log = tmp.path().join("t.db.log");
    File::create(&log).unwrap().set_len(10 * 1024 * 1024).unwrap();

    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    assert_eq!(fs::metadata(tmp.path().join("t.db.log.1")).unwrap().len(), 10 * 1024 * 1024);
    assert_eq!(log_lines(&db).len(), 2, "a fresh log holds just this run");
}

#[test]
fn db_drop_and_vacuum_are_logged_only_when_they_act() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s, "--no-log"]);

    db_cmd(tmp.path(), &["drop", "data", "--db", db_s]); // refused: no --yes
    db_cmd(tmp.path(), &["drop", "elsewhere", "--yes", "--db", db_s]); // no such index
    db_cmd(tmp.path(), &["info", "--db", db_s]);
    db_cmd(tmp.path(), &["check", "--db", db_s]);
    assert!(log_lines(&db).is_empty(), "nothing changed, so nothing logged: {:?}", log_lines(&db));

    db_cmd(tmp.path(), &["drop", "data", "--yes", "--db", db_s]);
    db_cmd(tmp.path(), &["vacuum", "--db", db_s]);
    let lines = log_lines(&db);
    let kinds: Vec<&str> = lines.iter().map(|(_, k, _)| k.as_str()).collect();
    assert_eq!(kinds, ["start", "end", "start", "end"], "{:?}", lines);
    assert!(lines[0].2.starts_with("db drop data --yes --db "), "{}", lines[0].2);
    assert!(lines[1].2.contains("— removed the index of") && lines[1].2.contains("(2 files)"), "{}", lines[1].2);
    assert!(lines[3].2.contains("— vacuumed "), "{}", lines[3].2);
}

// ─── search and export (5.0) ─────────────────────────────────────────────────

struct Query {
    code: i32,
    stdout: String,
    stderr: String,
}

fn query(cwd: &Path, args: &[&str]) -> Query {
    let out = bin().args(args).current_dir(cwd).output().unwrap();
    Query {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// File names (not full paths) from `--paths` output, in output order
fn names(stdout: &str) -> Vec<String> {
    stdout.lines().map(|l| Path::new(l).file_name().unwrap().to_string_lossy().into_owned()).collect()
}

/// A small indexed share: varied sizes, types, ages, a duplicate and a hard link
fn sample_share(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let nas = tmp.path().join("nas");
    write(&nas.join("projects/render.MOV"), &"m".repeat(3000));
    write(&nas.join("backup/render.mov"), &"m".repeat(3000));
    write(&nas.join("projects/notes.pdf"), &"p".repeat(500));
    write(&nas.join("projects/notes-link.pdf"), "");
    fs::remove_file(nas.join("projects/notes-link.pdf")).unwrap();
    fs::hard_link(nas.join("projects/notes.pdf"), nas.join("projects/notes-link.pdf")).unwrap();
    write(&nas.join("logs/app1.log"), "one\n");
    write(&nas.join("logs/app2.log"), "two\n");
    write(&nas.join("README"), "readme\n");
    set_mtime(&nas.join("logs/app1.log"), days_ago(400));
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "nas", "--db", db.to_str().unwrap()]);
    (nas, db)
}

#[test]
fn search_filters() {
    let tmp = TempDir::new();
    let (_, db) = sample_share(&tmp);
    let db_s = db.to_str().unwrap();
    let find = |extra: &[&str]| {
        let mut args = vec!["search", "nas", "--paths", "--db", db_s];
        args.extend_from_slice(extra);
        let q = query(tmp.path(), &args);
        assert_eq!(q.code, 0, "{}", q.stderr);
        let mut n = names(&q.stdout);
        n.sort();
        n
    };
    assert_eq!(find(&["--ext", "MOV"]), ["render.MOV", "render.mov"], "extension match ignores case");
    assert_eq!(find(&["--ext", "none"]), ["README"]);
    assert_eq!(find(&["--name", "app*"]), ["app1.log", "app2.log"]);
    assert_eq!(find(&["--name", "RENDER.*"]), Vec::<String>::new(), "--name is case-sensitive");
    assert_eq!(find(&["--name", "RENDER.*", "--ignore-case"]), ["render.MOV", "render.mov"]);
    assert_eq!(find(&["--larger-than", "1K"]), ["render.MOV", "render.mov"]);
    assert_eq!(find(&["--smaller-than", "10"]), ["README", "app1.log", "app2.log"]);
    assert_eq!(find(&["--older-than", "1y"]), ["app1.log"]);
    assert_eq!(find(&["--newer-than", "30d", "--ext", "log"]), ["app2.log"]);
    assert_eq!(find(&["--under", "nas/logs"]), ["app1.log", "app2.log"]);
    assert_eq!(find(&["--duplicate"]), ["render.MOV", "render.mov"], "hard links are not duplicates");
}

#[test]
fn search_sorting_and_limit() {
    let tmp = TempDir::new();
    let (_, db) = sample_share(&tmp);
    let db_s = db.to_str().unwrap();

    let by_size = query(tmp.path(), &["search", "nas", "--sort", "size", "--paths", "--db", db_s]);
    assert_eq!(names(&by_size.stdout)[2..4], ["notes-link.pdf", "notes.pdf"]);
    let by_name = query(tmp.path(), &["search", "nas", "--sort", "name", "--paths", "--db", db_s]);
    assert_eq!(names(&by_name.stdout)[0], "README");

    let limited = query(tmp.path(), &["search", "nas", "--limit", "2", "--db", db_s]);
    assert_eq!(limited.stdout.lines().count(), 3, "header + 2 rows:\n{}", limited.stdout);
    assert!(limited.stdout.starts_with("     SIZE  MODIFIED          PATH\n"), "{}", limited.stdout);
    assert!(limited.stderr.contains("… 5 more — use --limit"), "note goes to stderr:\n{}", limited.stderr);
    assert!(limited.stderr.starts_with("index as of "), "{}", limited.stderr);
    assert!(!limited.stdout.contains("more") && !limited.stdout.contains("index as of"));

    let none = query(tmp.path(), &["search", "nas", "--ext", "xyz", "--db", db_s]);
    assert_eq!((none.code, none.stdout.as_str()), (0, ""));
    assert!(none.stderr.contains("no matching files"));
}

#[test]
fn search_scope_folder_all_paths_and_cross_share_duplicates() {
    let tmp = TempDir::new();
    let (nas, db) = sample_share(&tmp);
    let db_s = db.to_str().unwrap();
    let other = tmp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    fs::copy(nas.join("projects/notes.pdf"), other.join("notes-copy.pdf")).unwrap();
    run_in(tmp.path(), &["index", "other", "--db", db_s]);

    let folder = query(tmp.path(), &["search", "nas/projects", "--paths", "--db", db_s]);
    assert_eq!(names(&folder.stdout).len(), 3, "a folder inside an index searches just that folder");

    let one = query(tmp.path(), &["search", "nas", "--duplicate", "--ext", "pdf", "--paths", "--db", db_s]);
    assert!(one.stdout.is_empty(), "within /nas the pdfs are hard links, not copies");
    let all = query(tmp.path(), &["search", "--duplicate", "--ext", "pdf", "--paths", "--db", db_s]);
    assert_eq!(names(&all.stdout).len(), 3, "across shares the copy in other/ makes them duplicates");
    assert!(all.stderr.contains("2 indexed paths"), "{}", all.stderr);

    let missing = query(tmp.path(), &["search", "/nowhere", "--db", db_s]);
    assert_eq!(missing.code, 1);
    assert!(missing.stderr.contains("no index covers /nowhere"), "{}", missing.stderr);
}

#[test]
fn search_and_export_formats() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("plain.txt"), "plain\n");
    write(&data.join("comma, \"quoted\".txt"), "odd\n");
    write(&data.join("tab\there.txt"), "tab\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    let csv = query(tmp.path(), &["export", "data", "--db", db_s]);
    let mut lines = csv.stdout.lines();
    assert_eq!(lines.next().unwrap(), "root,path,filename,directory,extension,size,modified,accessed,created,checksum,device,inode");
    assert!(csv.stdout.contains(",\"comma, \"\"quoted\"\".txt\","), "CSV quoting:\n{}", csv.stdout);
    assert!(csv.stdout.contains(",txt,6,"), "{}", csv.stdout);
    assert!(csv.stderr.contains("exported 3 files"));

    let tsv = query(tmp.path(), &["export", "data", "--format", "tsv", "--db", db_s]);
    assert!(tsv.stdout.contains("\ttab\\there.txt\t"), "TSV escapes tabs:\n{}", tsv.stdout);
    assert_eq!(tsv.stdout.lines().count(), 4);

    let json = query(tmp.path(), &["search", "data", "--name", "plain*", "--format", "json", "--db", db_s]);
    let obj = json.stdout.trim_end();
    assert_eq!(json.stdout.lines().count(), 1, "one object per line");
    assert!(obj.starts_with("{\"root\":\"") && obj.ends_with('}'), "{}", obj);
    assert!(obj.contains("\"filename\":\"plain.txt\"") && obj.contains("\"size\":6,"), "{}", obj);
    assert!(obj.contains("\"modified\":\"") && obj.contains("Z\",\"accessed\""), "ISO 8601 UTC dates: {}", obj);

    let nul = query(tmp.path(), &["search", "data", "-0", "--db", db_s]);
    assert_eq!(nul.stdout.matches('\0').count(), 3);
    assert!(!nul.stdout.contains('\n'), "NUL-separated, no newlines");
}

#[test]
fn export_has_no_row_limit() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 250);
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    let search = query(tmp.path(), &["search", "data", "--paths", "--db", db.to_str().unwrap()]);
    assert_eq!(search.stdout.lines().count(), 100, "search defaults to 100 rows");
    let export = query(tmp.path(), &["export", "data", "--db", db.to_str().unwrap()]);
    assert_eq!(export.stdout.lines().count(), 251, "header + every row");
}

#[test]
fn queries_never_log_migrate_or_create_anything() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a.JPG"), "a\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s, "--no-log"]);
    // Back to a v4 database: mixed-case extension, schema version 0
    let conn = Connection::open(&db).unwrap();
    conn.execute(&format!("UPDATE {} SET extension = 'JPG'", table_for(&data)), []).unwrap();
    conn.pragma_update(None, "user_version", 0).unwrap();
    drop(conn);

    let q = query(tmp.path(), &["search", "--ext", "jpg", "--paths", "--db", db_s]);
    assert_eq!(names(&q.stdout), ["a.JPG"], "un-migrated databases still match case-insensitively");
    query(tmp.path(), &["export", "--db", db_s]);
    assert_eq!(user_version(&db), 0, "queries don't migrate");
    assert!(!tmp.path().join("t.db.log").exists(), "queries aren't logged");

    let missing = query(tmp.path(), &["search", "--db", "missing.db"]);
    assert_eq!(missing.code, 1);
    assert!(!tmp.path().join("missing.db").exists());
}

#[test]
fn queries_read_v3_tables() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a"), "aaa\n");
    write(&data.join("b"), "aaa\n");
    let db = tmp.path().join("t.db");
    create_v3_table(&db, "data/", &[("data/a", "same"), ("data/b", "same")]);

    let q = query(tmp.path(), &["search", "--duplicate", "--paths", "--db", db.to_str().unwrap()]);
    assert_eq!(q.code, 0, "{}", q.stderr);
    assert_eq!(names(&q.stdout), ["a", "b"]);
    assert!(q.stderr.contains("has no recorded runs"), "{}", q.stderr);
}

// ─── report (5.1) ────────────────────────────────────────────────────────────

fn report(tmp: &TempDir, db: &Path, extra: &[&str]) -> Query {
    let mut args = vec!["report"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--db", db.to_str().unwrap()]);
    query(tmp.path(), &args)
}

/// CSV body rows (header dropped), split into fields — only for fields without commas
fn csv_rows(stdout: &str) -> Vec<Vec<String>> {
    stdout.lines().skip(1).map(|l| l.split(',').map(str::to_string).collect()).collect()
}

#[test]
fn report_overview() {
    let tmp = TempDir::new();
    let (_, db) = sample_share(&tmp);
    let r = report(&tmp, &db, &["nas"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("  Files                  7 · 6.9 KB"), "{}", r.stdout);
    assert!(r.stdout.contains("  Duplicates             1 group · 2.9 KB wasted"), "{}", r.stdout);
    assert!(r.stdout.contains("  Not modified in 1y     1 file · 4 B"), "{}", r.stdout);
    for heading in ["Largest files", "Space by type", "Space by folder (depth 1)", "Duplicates"] {
        assert!(r.stdout.contains(&format!("\n{}\n", heading)), "missing {}:\n{}", heading, r.stdout);
    }
    assert!(r.stderr.starts_with("index as of"), "notes go to stderr: {}", r.stderr);
    assert!(!tmp.path().join("t.db.log").exists() || log_lines(&db).len() == 2, "reports aren't logged");
}

#[test]
fn report_space_by_type_and_folder() {
    let tmp = TempDir::new();
    let (_, db) = sample_share(&tmp);

    let types = report(&tmp, &db, &["nas", "--by-type", "--format", "csv"]);
    assert_eq!(types.stdout.lines().next(), Some("type,files,size,share_percent"));
    let rows = csv_rows(&types.stdout);
    let by: HashMap<&str, (&str, &str)> = rows.iter().map(|r| (r[0].as_str(), (r[1].as_str(), r[2].as_str()))).collect();
    assert_eq!(by["mov"], ("2", "6000"), "JPG/jpg-style case differences are one type");
    assert_eq!(by["pdf"], ("2", "1000"));
    assert_eq!(by["log"], ("2", "8"));
    assert_eq!(by[""], ("1", "7"), "no extension");
    assert_eq!(rows[0][0], "mov", "largest first");
    let table = report(&tmp, &db, &["nas", "--by-type"]);
    assert!(table.stdout.contains("(none)"), "{}", table.stdout);

    let folders = report(&tmp, &db, &["nas", "--by-directory", "--format", "csv"]);
    let names: Vec<String> = csv_rows(&folders.stdout)
        .iter()
        .map(|r| Path::new(&r[0]).file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["projects", "backup", "logs", "nas"], "depth 1, largest first; nas = files directly in it");

    let folder = report(&tmp, &db, &["nas/projects", "--by-directory", "--format", "csv"]);
    assert_eq!(csv_rows(&folder.stdout).len(), 1, "relative to the folder given:\n{}", folder.stdout);
}

#[test]
fn report_by_folder_depth() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a/b/c/deep.txt"), &"x".repeat(100));
    write(&data.join("a/b/mid.txt"), &"x".repeat(10));
    write(&data.join("a/top.txt"), "x");
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);
    let folders = |depth: &str| -> Vec<(String, String)> {
        let r = report(&tmp, &db, &["data", "--by-directory", "--depth", depth, "--format", "csv"]);
        csv_rows(&r.stdout)
            .into_iter()
            .map(|f| (f[0].trim_start_matches(&*data.to_string_lossy()).to_string(), f[2].clone()))
            .collect()
    };
    assert_eq!(folders("1"), [("/a".to_string(), "111".to_string())]);
    assert_eq!(folders("2"), [("/a/b".to_string(), "110".to_string()), ("/a".to_string(), "1".to_string())]);
    assert_eq!(folders("3")[0], ("/a/b/c".to_string(), "100".to_string()));
    assert_eq!(report(&tmp, &db, &["data", "--by-directory", "--depth", "0"]).code, 2, "depth starts at 1");
}

#[test]
fn report_stale_by_modified_or_access() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("old-modified.txt"), "old\n");
    write(&data.join("old-accessed.txt"), "acc\n");
    write(&data.join("fresh.txt"), "new\n");
    set_mtime(&data.join("old-modified.txt"), days_ago(400));
    let times = fs::FileTimes::new().set_accessed(days_ago(400));
    File::options().write(true).open(data.join("old-accessed.txt")).unwrap().set_times(times).unwrap();
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "data", "--db", db.to_str().unwrap()]);

    let modified = report(&tmp, &db, &["data", "--stale", "1y"]);
    assert!(modified.stdout.starts_with("Not modified in 1y: 1 of 3 files"), "{}", modified.stdout);
    assert!(modified.stdout.contains("old-modified.txt") && !modified.stdout.contains("old-accessed.txt"));
    assert!(modified.stdout.contains("— by folder (depth 1)") && modified.stdout.contains("— largest files"));

    let accessed = report(&tmp, &db, &["data", "--stale", "1y", "--stale-by", "access", "--format", "csv"]);
    let names: Vec<String> = csv_rows(&accessed.stdout).iter().map(|r| r[2].clone()).collect();
    assert!(names.contains(&"old-accessed.txt".to_string()), "{:?}", names);
    assert!(!names.contains(&"fresh.txt".to_string()));
}

/// Duplicate groups of known sizes: big (3 copies), mid (2 copies, one under backup/),
/// small (2 copies, .txt), wide (7 copies), and a pair that are only hard links
fn duplicate_share(tmp: &TempDir) -> PathBuf {
    let d = tmp.path().join("dups");
    for i in 0..3 {
        write(&d.join(format!("big{}.iso", i)), &"B".repeat(4000));
    }
    write(&d.join("mid.mov"), &"M".repeat(2000));
    write(&d.join("backup/mid-copy.mov"), &"M".repeat(2000));
    write(&d.join("small.txt"), &"s".repeat(100));
    write(&d.join("small-copy.TXT"), &"s".repeat(100));
    for i in 0..7 {
        write(&d.join(format!("wide/w{}.dat", i)), &"w".repeat(10));
    }
    write(&d.join("linked.bin"), &"L".repeat(5000));
    fs::hard_link(d.join("linked.bin"), d.join("linked-too.bin")).unwrap();
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "dups", "--db", db.to_str().unwrap()]);
    db
}

#[test]
fn report_duplicates_and_filters() {
    let tmp = TempDir::new();
    let db = duplicate_share(&tmp);
    let groups = |extra: &[&str]| -> Vec<(String, String, String)> {
        let mut args = vec!["dups", "--duplicates", "--format", "csv"];
        args.extend_from_slice(extra);
        let r = report(&tmp, &db, &args);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let mut seen = Vec::new();
        for f in csv_rows(&r.stdout) {
            let g = (f[2].clone(), f[3].clone(), f[4].clone()); // size, copies, wasted
            if !seen.contains(&g) {
                seen.push(g);
            }
        }
        seen
    };
    let g = |size: &str, copies: &str, wasted: &str| (size.to_string(), copies.to_string(), wasted.to_string());

    assert_eq!(
        groups(&[]),
        [g("4000", "3", "8000"), g("2000", "2", "2000"), g("100", "2", "100"), g("10", "7", "60")],
        "largest waste first; the hard-linked pair isn't a group"
    );
    assert_eq!(groups(&["--min-size", "1K"]), [g("4000", "3", "8000"), g("2000", "2", "2000")]);
    assert_eq!(groups(&["--min-copies", "3"]), [g("4000", "3", "8000"), g("10", "7", "60")]);
    assert_eq!(groups(&["--min-wasted", "2000"]), [g("4000", "3", "8000"), g("2000", "2", "2000")]);
    assert_eq!(groups(&["--ext", "txt"]), [g("100", "2", "100")], "extension match ignores case");

    // --under picks groups with a copy there, but lists every copy
    let under = report(&tmp, &db, &["dups", "--duplicates", "--under", "dups/backup", "--format", "csv"]);
    let paths: Vec<String> = csv_rows(&under.stdout).iter().map(|r| r[5].clone()).collect();
    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|p| p.ends_with("/dups/mid.mov")), "copy outside backup/ still listed: {:?}", paths);
}

#[test]
fn report_duplicates_table_paths_and_summary() {
    let tmp = TempDir::new();
    let db = duplicate_share(&tmp);
    let r = report(&tmp, &db, &["dups", "--duplicates"]);
    let out = &r.stdout;
    assert!(out.starts_with("Duplicates\n  7.8 KB wasted   big0.iso — 3.9 KB × 3 copies\n"), "{}", out);
    assert!(out.contains("    … and 2 more\n"), "7-copy group capped at 5 paths:\n{}", out);
    assert!(out.contains("  Duplicate groups       : 4\n"), "{}", out);
    assert!(out.contains("  Files that are copies  : 10\n"), "2 + 1 + 1 + 6:\n{}", out);
    assert!(out.contains("  Total wasted space     : 9.9 KB\n"), "8000 + 2000 + 100 + 60 bytes:\n{}", out);
    assert!(out.contains("  Largest group          : big0.iso — 3.9 KB × 3 copies (7.8 KB wasted)\n"), "{}", out);

    let all = report(&tmp, &db, &["dups", "--duplicates", "--all-paths"]);
    assert!(!all.stdout.contains("… and"), "{}", all.stdout);
    let limited = report(&tmp, &db, &["dups", "--duplicates", "--limit", "1"]);
    assert!(limited.stdout.starts_with("Duplicates — largest 1 of 4 groups\n"), "{}", limited.stdout);
    assert!(limited.stdout.contains("Duplicate groups       : 4"), "summary still covers every group");
}

#[test]
fn report_duplicates_mark_hard_links_within_a_group() {
    let tmp = TempDir::new();
    let d = tmp.path().join("d");
    write(&d.join("a.bin"), &"x".repeat(300));
    fs::hard_link(d.join("a.bin"), d.join("b-link.bin")).unwrap();
    write(&d.join("c-copy.bin"), &"x".repeat(300));
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "d", "--db", db.to_str().unwrap()]);
    let r = report(&tmp, &db, &["d", "--duplicates"]);
    assert!(r.stdout.contains("300 B wasted   a.bin — 300 B × 2 copies"), "{}", r.stdout);
    assert!(r.stdout.contains("b-link.bin  (hard link)"), "{}", r.stdout);
    assert!(!r.stdout.contains("c-copy.bin  (hard link)"), "{}", r.stdout);
}

#[test]
fn report_options_and_formats() {
    let tmp = TempDir::new();
    let (_, db) = sample_share(&tmp);

    let top = report(&tmp, &db, &["nas", "--top-files", "2"]);
    assert_eq!(top.stdout.lines().count(), 4, "heading, column header, 2 rows:\n{}", top.stdout);
    let top_default = report(&tmp, &db, &["nas", "--top-files", "--limit", "3"]);
    assert_eq!(top_default.stdout.lines().count(), 5, "{}", top_default.stdout);
    let both = report(&tmp, &db, &["nas", "--by-type", "--duplicates"]);
    assert!(both.stdout.contains("Space by type") && both.stdout.contains("Duplicate groups"));

    let overview_csv = report(&tmp, &db, &["nas", "--format", "csv"]);
    assert_eq!(overview_csv.code, 1);
    assert!(overview_csv.stderr.contains("choose one section"), "{}", overview_csv.stderr);
    let two_csv = report(&tmp, &db, &["nas", "--by-type", "--duplicates", "--format", "csv"]);
    assert_eq!(two_csv.code, 1);

    let json = report(&tmp, &db, &["nas", "--top-files", "1", "--format", "json"]);
    assert!(json.stdout.starts_with("{\"root\":") && json.stdout.contains("\"size\":3000,"), "{}", json.stdout);

    let needs = report(&tmp, &db, &["nas", "--min-size", "1MB"]);
    assert_eq!(needs.code, 2, "--min-size needs --duplicates");
    let missing = report(&tmp, &db, &["/nowhere"]);
    assert_eq!(missing.code, 1);
}

// ─── index / update --dry-run (5.2) ──────────────────────────────────────────

/// The first number on the dry-run summary line starting with `label`
fn dry_value(stdout: &str, label: &str) -> i64 {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("no '{}' line in:\n{}", label, stdout));
    let rest = line.trim_start()[label.len()..].trim_start();
    rest.split(' ').next().unwrap().replace(',', "").parse().unwrap()
}

fn dry(tmp: &TempDir, args: &[&str]) -> Query {
    let mut a = args.to_vec();
    a.push("--dry-run");
    let q = query(tmp.path(), &a);
    assert_eq!(q.code, 0, "{}", q.stderr);
    q
}

#[test]
fn dry_run_index_creates_and_logs_nothing() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 5);
    write(&data.join("clip.MOV"), &"m".repeat(2000));
    let q = dry(&tmp, &["index", "data", "--db", "t.db"]);
    assert!(q.stdout.starts_with("Dry run — index "), "{}", q.stdout);
    assert!(q.stdout.contains("no database at t.db yet"), "{}", q.stdout);
    assert_eq!(dry_value(&q.stdout, "Found"), 6);
    assert_eq!(dry_value(&q.stdout, "Would record"), 6);
    assert!(q.stdout.contains("Would read       up to 5 files"), "the five same-size files, not the clip: {}", q.stdout);
    assert!(q.stdout.contains("Would record, by type") && q.stdout.contains("  mov "), "{}", q.stdout);
    assert!(!tmp.path().join("t.db").exists(), "no database created");
    assert!(!tmp.path().join("t.db.log").exists(), "not logged");
}

#[test]
fn dry_run_index_predicts_a_resume() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 30);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);
    Connection::open(&db).unwrap().execute(&format!("DELETE FROM {} WHERE id % 3 = 0", table_for(&data)), []).unwrap();

    let q = dry(&tmp, &["index", "data", "--db", db_s]);
    let real = run_in(tmp.path(), &["index", "data", "--db", db_s]).summary;
    assert_eq!(dry_value(&q.stdout, "Already indexed") as usize, real.skipped);
    assert_eq!(dry_value(&q.stdout, "Would record") as usize, real.written);
}

#[test]
fn dry_run_update_predicts_the_real_update_and_changes_nothing() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data, 20);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    write(&data.join("f0"), "a longer replacement\n");
    write(&data.join("f1"), "file X\n");
    set_mtime(&data.join("f1"), days_ago(3));
    write(&data.join("new.txt"), "new\n");
    fs::remove_file(data.join("f2")).unwrap();
    fs::rename(data.join("f3"), data.join("f3-renamed")).unwrap();

    let t = table_for(&data);
    let snapshot = |db: &Path| {
        let conn = Connection::open(db).unwrap();
        let rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0)).unwrap();
        let started: String = conn.query_row("SELECT last_started FROM roots", [], |r| r.get(0)).unwrap();
        (rows, started, user_version(db))
    };
    let before = snapshot(&db);
    let log_before = log_lines(&db).len();

    let q = dry(&tmp, &["update", "data", "--db", db_s]);
    assert_eq!(snapshot(&db), before, "dry run leaves the database untouched");
    assert_eq!(log_lines(&db).len(), log_before, "and isn't logged");
    assert!(q.stdout.contains("(2 new, 2 changed)"), "new.txt + f3-renamed; f0 + f1:\n{}", q.stdout);

    let real = run_in(tmp.path(), &["update", "data", "--db", db_s]).summary;
    assert_eq!(dry_value(&q.stdout, "Unchanged") as usize, real.skipped);
    assert_eq!(dry_value(&q.stdout, "Would record") as usize, real.written);
    assert_eq!(dry_value(&q.stdout, "Would remove") as usize, real.removed);
    assert_eq!(dry_value(&q.stdout, "Found") as usize, real.found);
}

#[test]
fn dry_run_update_keeps_the_deletion_safeguards() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    distinct_files(&data.join("open"), 3);
    distinct_files(&data.join("locked"), 2);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    chmod(&data.join("locked"), "000");
    if permissions_ignored(&data.join("locked")) {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }
    let q = dry(&tmp, &["update", "data", "--db", db_s]);
    assert_eq!(dry_value(&q.stdout, "Would remove"), 0, "rows under an unreadable folder are kept");
    assert!(q.stdout.contains("Couldn't read    1 folder — rows under it are kept"), "{}", q.stdout);
    chmod(&data.join("locked"), "755");

    fs::rename(&data, tmp.path().join("moved")).unwrap();
    fs::create_dir(&data).unwrap();
    let empty = dry(&tmp, &["update", "data", "--db", db_s]);
    assert_eq!(dry_value(&empty.stdout, "Would remove"), 0);
    assert!(empty.stdout.contains("nothing would be removed: no files found"), "{}", empty.stdout);
}

#[test]
fn dry_run_reports_what_the_run_would_do_first() {
    let tmp = TempDir::new();
    let data = tmp.path().join("data");
    write(&data.join("a"), "aaa\n");
    write(&data.join("b"), "bbb\n");
    let db = tmp.path().join("t.db");
    create_v3_table(&db, "data/", &[("data/a", "sum-a")]);

    let q = dry(&tmp, &["index", "data/", "--db", db.to_str().unwrap()]);
    assert!(q.stdout.contains("would first update the database schema to version 5"), "{}", q.stdout);
    assert!(q.stdout.contains("would first migrate 1 rows from the table for \"data/\""), "{}", q.stdout);
    assert_eq!(dry_value(&q.stdout, "Already indexed"), 1, "migrated rows aren't re-hashed");
    assert_eq!(dry_value(&q.stdout, "Would record"), 1);
    assert_eq!(user_version(&db), 0, "nothing migrated by the dry run");

    Connection::open(&db).unwrap().pragma_update(None, "user_version", 99).unwrap();
    let newer = query(tmp.path(), &["index", "data", "--dry-run", "--db", db.to_str().unwrap()]);
    assert_eq!(newer.code, 1);
    assert!(newer.stderr.contains("newer than this diskunder supports"), "{}", newer.stderr);
}

// ─── File-type filters (5.2) ─────────────────────────────────────────────────

/// photos (jpg, one JPG), a clip, temp files, a log and a README
fn typed_share(tmp: &TempDir) -> PathBuf {
    let d = tmp.path().join("share");
    for i in 0..3 {
        write(&d.join(format!("p{}.jpg", i)), &format!("photo {}\n", i));
    }
    write(&d.join("SHOUTY.JPG"), "loud\n");
    write(&d.join("clip.mov"), &"m".repeat(500));
    for i in 0..4 {
        write(&d.join(format!("cache/t{}.tmp", i)), &format!("tmp {}\n", i));
    }
    write(&d.join("run.log"), "log\n");
    write(&d.join("README"), "readme\n");
    d
}

fn stored_filters(db: &Path) -> (Option<String>, Option<String>) {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT include_ext, exclude_ext FROM roots", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
}

fn indexed_names(db: &Path, table: &str) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn.prepare(&format!("SELECT filename FROM {} ORDER BY filename", table)).unwrap();
    let names = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    names
}

#[test]
fn excluded_types_are_never_read_and_the_filters_are_stored() {
    let tmp = TempDir::new();
    let d = typed_share(&tmp);
    chmod(&d.join("cache/t0.tmp"), "000"); // would be an "unreadable" error if it were read
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();

    let run = run_in(tmp.path(), &["index", "share", "--exclude-ext", "TMP,.log", "--db", db_s]);
    assert_eq!((run.summary.found, run.summary.written), (6, 6));
    assert_eq!(run.summary.errors, "0 errors", "the unreadable .tmp file was never opened");
    assert!(run.stderr.contains("file-type filters: exclude log,tmp"), "{}", run.stderr);
    assert!(run.stderr.contains("\n5 excluded by filters\n"), "own line, after the summary:\n{}", run.stderr);
    assert_eq!(stored_filters(&db), (None, Some("log,tmp".to_string())));
    assert!(!indexed_names(&db, &table_for(&d)).iter().any(|n| n.ends_with(".tmp") || n.ends_with(".log")));

    let again = run_in(tmp.path(), &["update", "share", "--db", db_s]);
    assert!(again.stderr.contains("file-type filters: exclude log,tmp"), "stored filters reused:\n{}", again.stderr);
    assert_eq!((again.summary.skipped, again.summary.written, again.summary.removed), (6, 0, 0));
    assert!(log_lines(&db).last().unwrap().2.ends_with(", 5 excluded by filters"), "{:?}", log_lines(&db).last());
    chmod(&d.join("cache/t0.tmp"), "644");
}

#[test]
fn include_is_an_allowlist_with_a_none_token() {
    let tmp = TempDir::new();
    let d = typed_share(&tmp);
    let db = tmp.path().join("t.db");
    run_in(tmp.path(), &["index", "share", "--include-ext", "jpg,none", "--db", db.to_str().unwrap()]);
    assert_eq!(
        indexed_names(&db, &table_for(&d)),
        ["README", "SHOUTY.JPG", "p0.jpg", "p1.jpg", "p2.jpg"],
        "case-insensitive, and none = no extension"
    );
    assert_eq!(stored_filters(&db), (Some("none,jpg".to_string()), None));
}

#[test]
fn changing_filters_removes_now_excluded_rows_separately_from_deleted_ones() {
    let tmp = TempDir::new();
    let d = typed_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--db", db_s]);
    fs::remove_file(d.join("p0.jpg")).unwrap();

    // index never removes rows — it says what update would do
    let idx = run_in(tmp.path(), &["index", "share", "--exclude-ext", "tmp", "--db", db_s]);
    assert!(idx.stderr.contains("file-type filters changed: all file types → exclude tmp"), "{}", idx.stderr);
    assert!(idx.stderr.contains("4 indexed files are now excluded by the filters — `update` removes their rows"), "{}", idx.stderr);
    assert_eq!(indexed_names(&db, &table_for(&d)).len(), 11);

    let upd = run_in(tmp.path(), &["update", "share", "--db", db_s]);
    assert_eq!(upd.summary.removed, 1, "only the deleted p0.jpg counts as removed");
    assert!(upd.stderr.contains("\n4 excluded by filters, 4 rows removed as excluded\n"), "{}", upd.stderr);
    assert_eq!(indexed_names(&db, &table_for(&d)).len(), 6);
}

#[test]
fn all_types_clears_the_filters() {
    let tmp = TempDir::new();
    let d = typed_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--include-ext", "mov", "--db", db_s]);
    let all = run_in(tmp.path(), &["update", "share", "--all-types", "--db", db_s]);
    assert!(all.stderr.contains("file-type filters changed: include mov → all file types"), "{}", all.stderr);
    assert_eq!(all.summary.written, 10);
    assert_eq!(stored_filters(&db), (None, None));
    assert_eq!(indexed_names(&db, &table_for(&d)).len(), 11);

    let both = query(tmp.path(), &["index", "share", "--all-types", "--exclude-ext", "tmp", "--db", db_s]);
    assert_eq!(both.code, 2, "--all-types can't be combined with other filters");
}

#[test]
fn an_all_excluded_walk_still_removes_deleted_files() {
    let tmp = TempDir::new();
    let d = typed_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--db", db_s]);
    fs::remove_file(d.join("README")).unwrap();

    // Nothing is included, but excluded files prove the share is mounted
    let run = run_in(tmp.path(), &["update", "share", "--include-ext", "xyz", "--db", db_s]);
    assert!(!run.stderr.contains("is the share mounted"), "{}", run.stderr);
    assert_eq!(run.summary.removed, 1);
    assert!(run.stderr.contains("10 rows removed as excluded"), "{}", run.stderr);
}

#[test]
fn dry_run_predicts_filter_changes() {
    let tmp = TempDir::new();
    let d = typed_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--db", db_s]);
    fs::remove_file(d.join("p1.jpg")).unwrap();

    let q = dry(&tmp, &["update", "share", "--include-ext", "jpg", "--db", db_s]);
    assert!(q.stdout.contains("file-type filters would change: all file types → include jpg"), "{}", q.stdout);
    assert!(q.stdout.contains("Excluded by filters, by type"), "{}", q.stdout);
    assert_eq!(stored_filters(&db), (None, None), "dry run stores nothing");

    let real = run_in(tmp.path(), &["update", "share", "--include-ext", "jpg", "--db", db_s]);
    assert_eq!(dry_value(&q.stdout, "Found") as usize, real.summary.found);
    assert_eq!(dry_value(&q.stdout, "Excluded"), 7, "clip, 4 tmp, log, README");
    assert_eq!(dry_value(&q.stdout, "Would remove") as usize, real.summary.removed);
    assert_eq!(dry_value(&q.stdout, "Now excluded"), 7);
    assert!(real.stderr.contains("7 rows removed as excluded"), "{}", real.stderr);
}

#[test]
fn filters_show_in_db_info_and_query_notes() {
    let tmp = TempDir::new();
    typed_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--exclude-ext", "tmp", "--db", db_s]);

    let info = db_cmd(tmp.path(), &["info", "--db", db_s]);
    assert!(info.stdout.contains("  filters     exclude tmp\n"), "{}", info.stdout);
    let note = "is indexed with file-type filters (exclude tmp) — other types aren't in the index";
    assert!(query(tmp.path(), &["search", "share", "--db", db_s]).stderr.contains(note));
    assert!(query(tmp.path(), &["report", "share", "--db", db_s]).stderr.contains(note));
}

#[test]
fn schema_1_databases_are_read_and_upgraded() {
    let tmp = TempDir::new();
    typed_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--db", db_s]);
    // Back to a v5.1 database: no filter columns on roots, schema version 1
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("ALTER TABLE roots DROP COLUMN include_ext; ALTER TABLE roots DROP COLUMN exclude_ext;").unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    drop(conn);

    assert_eq!(db_cmd(tmp.path(), &["info", "--db", db_s]).code, 0, "read-only commands cope");
    assert_eq!(query(tmp.path(), &["search", "share", "--db", db_s]).code, 0);
    let upd = run_in(tmp.path(), &["update", "share", "--exclude-ext", "tmp", "--db", db_s]);
    assert!(upd.stderr.contains("schema 2: added file-type filter columns to the roots table"), "{}", upd.stderr);
    assert_eq!(user_version(&db), 5);
    assert_eq!(stored_filters(&db), (None, Some("tmp".to_string())));
}

// ─── Path filters (5.3) ──────────────────────────────────────────────────────

/// A NAS-like share: snapshots, Synology thumbnails, a render cache with an exception,
/// node_modules, and a capitalised "Snapshot" folder that should be kept
fn nas_share(tmp: &TempDir) -> PathBuf {
    let d = tmp.path().join("share");
    for f in [
        "photos/a.jpg",
        "photos/b.jpg",
        "photos/@eaDir/a.jpg/thumb.jpg",
        ".snapshot/daily/photos/a.jpg",
        "projects/film/render-cache/f1.exr",
        "projects/film/render-cache/f2.exr",
        "projects/film/render-cache/keep/final.exr",
        "projects/film/edit.prproj",
        "web/node_modules/x/index.js",
        "web/app.js",
        "Snapshot/real.txt",
    ] {
        write(&d.join(f), &format!("{}\n", f));
    }
    d
}

const NAS_FILTERS: [&str; 10] = [
    "--exclude-path", ".snapshot",
    "--exclude-path", "@eaDir",
    "--exclude-path", "projects/*/render-cache",
    "--exclude-path", "**/node_modules",
    "--include-path", "projects/*/render-cache/keep",
];

fn rel_paths(db: &Path, share: &Path) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn.prepare(&format!("SELECT path FROM {} ORDER BY path", table_for(share))).unwrap();
    let prefix = format!("{}/", share.display());
    let paths = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| p.unwrap().trim_start_matches(&prefix).to_string())
        .collect();
    paths
}

fn index_args<'a>(cmd: &'a str, db: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![cmd, "share", "--db", db];
    args.extend_from_slice(extra);
    args
}

#[test]
fn path_patterns_skip_folders_and_files_with_exceptions() {
    let tmp = TempDir::new();
    let d = nas_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    let run = run_in(tmp.path(), &index_args("index", db_s, &NAS_FILTERS));
    assert_eq!(
        rel_paths(&db, &d),
        [
            "Snapshot/real.txt",
            "photos/a.jpg",
            "photos/b.jpg",
            "projects/film/edit.prproj",
            "projects/film/render-cache/keep/final.exr",
            "web/app.js",
        ],
        "case-sensitive names, anchored wildcards, ** and an exception"
    );
    assert!(run.stderr.contains("\n2 excluded by filters, 3 folders skipped\n"), "{}", run.stderr);
    assert!(run.stderr.contains("path filters: exclude .snapshot, @eaDir, projects/*/render-cache, **/node_modules · except projects/*/render-cache/keep"), "{}", run.stderr);
}

#[test]
fn excluded_folders_are_never_entered() {
    let tmp = TempDir::new();
    let d = nas_share(&tmp);
    chmod(&d.join(".snapshot"), "000");
    if permissions_ignored(&d.join(".snapshot")) {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }
    let db = tmp.path().join("t.db");
    let run = run_in(tmp.path(), &index_args("index", db.to_str().unwrap(), &["--exclude-path", ".snapshot"]));
    assert_eq!(run.summary.errors, "0 errors", "the locked .snapshot folder was never opened");
    chmod(&d.join(".snapshot"), "755");
}

#[test]
fn path_and_type_filters_are_stored_and_replaced_independently() {
    let tmp = TempDir::new();
    nas_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &index_args("index", db_s, &["--exclude-ext", "exr", "--exclude-path", ".snapshot"]));

    let paths_only = run_in(tmp.path(), &index_args("update", db_s, &["--exclude-path", "@eaDir"]));
    assert!(paths_only.stderr.contains("file-type filters: exclude exr"), "type filters kept:\n{}", paths_only.stderr);
    assert!(paths_only.stderr.contains("path filters changed: exclude .snapshot → exclude @eaDir"), "{}", paths_only.stderr);

    let cleared = run_in(tmp.path(), &index_args("update", db_s, &["--no-path-filters"]));
    assert!(cleared.stderr.contains("path filters changed: exclude @eaDir → no path filters"), "{}", cleared.stderr);
    let (exclude_paths, include_paths): (Option<String>, Option<String>) = Connection::open(&db)
        .unwrap()
        .query_row("SELECT exclude_paths, include_paths FROM roots", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((exclude_paths, include_paths), (None, None));
    assert_eq!(stored_filters(&db), (None, Some("exr".to_string())));

    assert_eq!(query(tmp.path(), &index_args("index", db_s, &["--no-path-filters", "--exclude-path", "x"])).code, 2);
    assert_eq!(query(tmp.path(), &index_args("index", db_s, &["--exclude-path", "a/[b"])).code, 2, "invalid pattern");
}

#[test]
fn rows_under_newly_excluded_folders_are_excluded_not_deleted() {
    let tmp = TempDir::new();
    let d = nas_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &index_args("index", db_s, &[]));
    fs::remove_file(d.join("web/app.js")).unwrap();

    let idx = run_in(tmp.path(), &index_args("index", db_s, &["--exclude-path", ".snapshot", "--exclude-path", "photos"]));
    assert!(idx.stderr.contains("4 indexed files are now excluded by the filters — `update` removes their rows"), "{}", idx.stderr);
    assert_eq!(rel_paths(&db, &d).len(), 11, "index never removes rows");

    let upd = run_in(tmp.path(), &index_args("update", db_s, &[]));
    assert_eq!(upd.summary.removed, 1, "only the deleted web/app.js");
    assert!(upd.stderr.contains("4 rows removed as excluded"), "{}", upd.stderr);
    assert!(!rel_paths(&db, &d).iter().any(|p| p.starts_with("photos/") || p.starts_with(".snapshot/")));
}

#[test]
fn include_path_alone_does_nothing_and_says_so() {
    let tmp = TempDir::new();
    nas_share(&tmp);
    let db = tmp.path().join("t.db");
    let run = run_in(tmp.path(), &index_args("index", db.to_str().unwrap(), &["--include-path", "photos"]));
    assert_eq!(run.summary.written, 11);
    assert!(run.stderr.contains("--include-path only makes exceptions to --exclude-path"), "{}", run.stderr);
}

#[test]
fn dry_run_predicts_path_filter_changes() {
    let tmp = TempDir::new();
    let d = nas_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &index_args("index", db_s, &[]));
    fs::remove_file(d.join("web/app.js")).unwrap();

    let q = dry(&tmp, &index_args("update", db_s, &NAS_FILTERS));
    assert!(q.stdout.contains("path filters would change: no path filters → exclude .snapshot"), "{}", q.stdout);
    assert!(q.stdout.contains(", 3 folders not entered"), "{}", q.stdout);
    let real = run_in(tmp.path(), &index_args("update", db_s, &NAS_FILTERS));
    assert_eq!(dry_value(&q.stdout, "Found") as usize, real.summary.found);
    assert_eq!(dry_value(&q.stdout, "Would remove") as usize, real.summary.removed);
    let now_excluded = dry_value(&q.stdout, "Now excluded");
    assert!(real.stderr.contains(&format!("{} rows removed as excluded", now_excluded)), "{} vs\n{}", now_excluded, real.stderr);
}

#[test]
fn path_filters_show_in_db_info_and_queries_and_schema_2_upgrades() {
    let tmp = TempDir::new();
    nas_share(&tmp);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &index_args("index", db_s, &["--exclude-path", ".snapshot"]));

    assert!(db_cmd(tmp.path(), &["info", "--db", db_s]).stdout.contains("  paths       exclude .snapshot\n"));
    assert!(query(tmp.path(), &["search", "share", "--db", db_s])
        .stderr
        .contains("is indexed with path filters (exclude .snapshot) — matching paths aren't in the index"));

    // Back to a v5.2 database: no path filter columns, schema version 2
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("ALTER TABLE roots DROP COLUMN exclude_paths; ALTER TABLE roots DROP COLUMN include_paths;").unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    drop(conn);
    assert_eq!(db_cmd(tmp.path(), &["info", "--db", db_s]).code, 0, "read-only commands cope");
    let upd = run_in(tmp.path(), &index_args("update", db_s, &["--exclude-path", "@eaDir"]));
    assert!(upd.stderr.contains("schema 3: added path filter columns to the roots table"), "{}", upd.stderr);
    assert_eq!(user_version(&db), 5);
}

// ─── Jobs (5.4) ──────────────────────────────────────────────────────────────

/// Runs diskunder against a temporary jobs store (never the real ~/.config), stdin closed
fn jobs_cmd(tmp: &TempDir, args: &[&str]) -> Query {
    let store = tmp.path().join("store/diskunder.db");
    let out = bin()
        .args(args)
        .args(["--config-db", store.to_str().unwrap(), "--no-tui"])
        .current_dir(tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    Query {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn ok(q: Query) -> Query {
    assert_eq!(q.code, 0, "stdout:\n{}\nstderr:\n{}", q.stdout, q.stderr);
    q
}

fn store(tmp: &TempDir) -> Connection {
    Connection::open(tmp.path().join("store/diskunder.db")).unwrap()
}

/// A share with photos, a temp file and a snapshot folder, and a job over it
fn job_share(tmp: &TempDir) -> PathBuf {
    let d = tmp.path().join("share");
    for i in 0..3 {
        write(&d.join(format!("p{}.jpg", i)), &format!("photo {}\n", i));
    }
    write(&d.join("x.tmp"), "tmp\n");
    write(&d.join(".snapshot/old.jpg"), "old\n");
    ok(jobs_cmd(tmp, &[
        "jobs", "add", "nightly", "update", "share", "--db", "idx/index.db", "--threads", "2",
        "--exclude-ext", "tmp", "--exclude-path", ".snapshot",
    ]));
    d
}

#[test]
fn jobs_add_list_show() {
    let tmp = TempDir::new();
    let empty = ok(jobs_cmd(&tmp, &["jobs", "list"]));
    assert!(empty.stdout.starts_with("no jobs yet"), "{}", empty.stdout);
    assert!(!tmp.path().join("store").exists(), "listing doesn't create the store");

    let d = job_share(&tmp);
    let list = ok(jobs_cmd(&tmp, &["jobs", "list"]));
    assert!(list.stdout.contains("NAME") && list.stdout.contains("nightly") && list.stdout.contains("never"), "{}", list.stdout);

    let show = ok(jobs_cmd(&tmp, &["jobs", "show", "nightly"]));
    assert!(show.stdout.contains(&format!("  path       {}\n", d.display())), "stored absolute:\n{}", show.stdout);
    assert!(show.stdout.contains(&format!("  database   {}  (not created yet)\n", tmp.path().join("idx/index.db").display())), "{}", show.stdout);
    assert!(show.stdout.contains("  threads    2\n") && show.stdout.contains("  filters    exclude tmp · exclude .snapshot\n"), "{}", show.stdout);
    assert!(show.stdout.contains("never run — `diskunder run nightly`"), "{}", show.stdout);
}

#[test]
fn run_uses_the_job_and_records_history() {
    let tmp = TempDir::new();
    let d = job_share(&tmp);
    fs::create_dir_all(tmp.path().join("idx")).unwrap();
    let db = tmp.path().join("idx/index.db");

    // Run from a different working directory: the job's paths are absolute
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let store_path = tmp.path().join("store/diskunder.db");
    let out = bin()
        .args(["run", "nightly", "--no-tui", "--config-db", store_path.to_str().unwrap()])
        .current_dir(&elsewhere)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{}", stderr);
    let s = parse_summary(&stderr);
    assert_eq!((s.found, s.written), (3, 3), "the job's filters applied:\n{}", stderr);
    assert!(stderr.contains("1 excluded by filters, 1 folder skipped"), "{}", stderr);
    assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM {}", table_for(&d))), 3);

    let lines = log_lines(&db);
    assert!(lines[0].2.starts_with("run nightly --no-tui"), "run log beside the job's database: {:?}", lines);
    let (run_id, outcome, found, written): (String, String, i64, i64) = store(&tmp)
        .query_row("SELECT run_id, outcome, found, written FROM job_runs", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap();
    assert_eq!((outcome.as_str(), found, written), ("completed", 3, 3));
    assert_eq!(run_id, lines[0].0, "history and run log share the run id");

    write(&d.join("new.jpg"), "new\n");
    ok(jobs_cmd(&tmp, &["run", "nightly"]));
    let list = ok(jobs_cmd(&tmp, &["jobs", "list"]));
    assert!(list.stdout.contains("completed") && list.stdout.contains("  4  "), "{}", list.stdout);
    let show = ok(jobs_cmd(&tmp, &["jobs", "show", "nightly"]));
    assert_eq!(show.stdout.matches("completed").count(), 2, "two runs in history:\n{}", show.stdout);
    assert!(show.stdout.contains("Last run: 4 found, 3 unchanged, 1 written"), "{}", show.stdout);
}

#[test]
fn run_records_errors_and_failures() {
    let tmp = TempDir::new();
    let d = job_share(&tmp);
    distinct_files(&d.join("locked"), 1);
    chmod(&d.join("locked"), "000");
    if permissions_ignored(&d.join("locked")) {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }
    ok(jobs_cmd(&tmp, &["run", "nightly"]));
    chmod(&d.join("locked"), "755");
    let show = ok(jobs_cmd(&tmp, &["jobs", "show", "nightly"]));
    assert!(show.stdout.contains("Errors in the last run\n") && show.stdout.contains("/locked — "), "{}", show.stdout);

    // A run that fails before indexing anything still lands in the history, with why
    fs::rename(&d, tmp.path().join("gone")).unwrap();
    let failed = jobs_cmd(&tmp, &["run", "nightly"]);
    assert_eq!(failed.code, 1);
    let show = ok(jobs_cmd(&tmp, &["jobs", "show", "nightly"]));
    assert!(show.stdout.contains("failed (exit 1)"), "{}", show.stdout);
    assert!(show.stdout.contains("path not found"), "{}", show.stdout);
}

#[test]
fn run_dry_run_changes_nothing() {
    let tmp = TempDir::new();
    job_share(&tmp);
    let q = ok(jobs_cmd(&tmp, &["run", "nightly", "--dry-run"]));
    assert!(q.stdout.starts_with("Dry run — update "), "{}", q.stdout);
    assert_eq!(dry_value(&q.stdout, "Would record"), 3, "the job's filters apply to the test");
    assert!(!tmp.path().join("idx/index.db").exists());
    let runs: i64 = store(&tmp).query_row("SELECT COUNT(*) FROM job_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(runs, 0, "a dry run isn't a run");
}

#[test]
fn one_run_per_job_at_a_time() {
    let tmp = TempDir::new();
    job_share(&tmp);
    fs::create_dir_all(tmp.path().join("idx")).unwrap();
    let lock_path = tmp.path().join("store/locks/nightly.lock");
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let lock = File::create(&lock_path).unwrap();
    lock.lock().unwrap();

    let second = jobs_cmd(&tmp, &["run", "nightly"]);
    assert_eq!(second.code, 1);
    assert!(second.stderr.contains("job nightly is already running"), "{}", second.stderr);

    // A run row with no finish shows as running while the lock is held, then as interrupted
    store(&tmp)
        .execute("INSERT INTO job_runs (job, run_id, started, outcome) VALUES ('nightly', 'abc123', datetime('now'), 'running')", [])
        .unwrap();
    assert!(ok(jobs_cmd(&tmp, &["jobs", "list"])).stdout.contains("running"));
    drop(lock);
    assert!(ok(jobs_cmd(&tmp, &["jobs", "list"])).stdout.contains("did not finish"));
    ok(jobs_cmd(&tmp, &["run", "nightly"]));
}

#[test]
fn jobs_edit_changes_only_what_is_given() {
    let tmp = TempDir::new();
    job_share(&tmp);
    let e = ok(jobs_cmd(&tmp, &["jobs", "edit", "nightly", "--threads", "8", "--exclude-path", "cache"]));
    assert!(e.stdout.contains("  threads   2 → 8\n"), "{}", e.stdout);
    assert!(e.stdout.contains("  filters   exclude tmp · exclude .snapshot → exclude tmp · exclude cache\n"), "path filters replaced, type filters kept:\n{}", e.stdout);
    assert!(!e.stdout.contains("database"), "{}", e.stdout);

    let mode = ok(jobs_cmd(&tmp, &["jobs", "edit", "nightly", "--mode", "index", "--all-types"]));
    assert!(mode.stdout.contains("  mode      update → index\n"), "{}", mode.stdout);
    let same = ok(jobs_cmd(&tmp, &["jobs", "edit", "nightly", "--threads", "8"]));
    assert!(same.stdout.contains("job nightly unchanged"), "{}", same.stdout);
    assert_eq!(jobs_cmd(&tmp, &["jobs", "edit", "nope", "--threads", "1"]).code, 1);
}

#[test]
fn jobs_names_and_arguments_are_checked() {
    let tmp = TempDir::new();
    job_share(&tmp);
    let bad = jobs_cmd(&tmp, &["jobs", "add", "bad/name", "index", "share"]);
    assert_eq!(bad.code, 1);
    assert!(bad.stderr.contains("letters, digits, - and _"), "{}", bad.stderr);
    let dup = jobs_cmd(&tmp, &["jobs", "add", "nightly", "index", "share"]);
    assert!(dup.code == 1 && dup.stderr.contains("already exists"), "{}", dup.stderr);
    let with_db = jobs_cmd(&tmp, &["run", "nightly", "--db", "other.db"]);
    assert_eq!(with_db.code, 2, "a job always uses its own database");
    let no_name = jobs_cmd(&tmp, &["run"]);
    assert_eq!(no_name.code, 1);
    assert!(no_name.stdout.contains("nightly") && no_name.stderr.contains("give a job name"), "{}", no_name.stderr);
    let unknown = jobs_cmd(&tmp, &["run", "nope"]);
    assert!(unknown.code == 1 && unknown.stderr.contains("no job named nope"), "{}", unknown.stderr);
    assert_eq!(jobs_cmd(&tmp, &["jobs", "add", "x", "sometimes", "share"]).code, 2, "mode is index or update");
}

#[test]
fn jobs_remove_asks_and_can_drop_the_index() {
    let tmp = TempDir::new();
    let d = job_share(&tmp);
    fs::create_dir_all(tmp.path().join("idx")).unwrap();
    let db = tmp.path().join("idx/index.db");
    ok(jobs_cmd(&tmp, &["run", "nightly"]));

    let refused = jobs_cmd(&tmp, &["jobs", "remove", "nightly"]);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("pass --yes"), "{}", refused.stderr);

    let removed = ok(jobs_cmd(&tmp, &["jobs", "remove", "nightly", "--yes", "--drop-index"]));
    assert!(removed.stdout.contains("removed job nightly and its run history"), "{}", removed.stdout);
    assert!(removed.stdout.contains("removed the index of") && removed.stdout.contains("(3 files)"), "{}", removed.stdout);
    let s = store(&tmp);
    let left: (i64, i64) = s
        .query_row("SELECT (SELECT COUNT(*) FROM jobs), (SELECT COUNT(*) FROM job_runs)", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(left, (0, 0));
    assert!(!tables(&db).contains(&table_for(&d)), "index dropped");
    assert!(log_lines(&db).iter().any(|(_, k, t)| k == "end" && t.contains("removed the index of")), "drop is logged");
}

#[test]
fn the_database_folder_is_created_if_missing() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    let run = run_in(tmp.path(), &["index", "data", "--db", "new/folder/t.db"]);
    assert_eq!(run.summary.written, 2);
    assert!(tmp.path().join("new/folder/t.db").exists() && tmp.path().join("new/folder/t.db.log").exists());
}

// ─── Settings and backup (5.5.3) ─────────────────────────────────────────────

#[test]
fn settings_list_set_and_unset() {
    let tmp = TempDir::new();
    let list = ok(jobs_cmd(&tmp, &["settings"]));
    assert!(list.stdout.contains("log.rotate_mb") && list.stdout.contains("10 (default)"), "{}", list.stdout);
    assert!(list.stdout.contains("types.images") && list.stdout.contains("(built in)"), "{}", list.stdout);
    assert!(!tmp.path().join("store").exists(), "listing doesn't create the store");

    assert!(ok(jobs_cmd(&tmp, &["settings", "set", "defaults.threads", "8"])).stdout.contains("defaults.threads = 8"));
    let bad = jobs_cmd(&tmp, &["settings", "set", "defaults.threads", "lots"]);
    assert!(bad.code == 1 && bad.stderr.contains("whole number"), "{}", bad.stderr);
    assert_eq!(jobs_cmd(&tmp, &["settings", "set", "no.such", "1"]).code, 1);
    ok(jobs_cmd(&tmp, &["settings", "set", "types.raw", "CR2, nef"]));
    let list = ok(jobs_cmd(&tmp, &["settings"]));
    assert!(list.stdout.contains("types.raw") && list.stdout.contains("cr2,nef"), "{}", list.stdout);
    ok(jobs_cmd(&tmp, &["settings", "unset", "defaults.threads"]));
    assert!(ok(jobs_cmd(&tmp, &["settings"])).stdout.contains("defaults.threads"));
}

#[test]
fn type_groups_work_in_every_type_filter() {
    let tmp = TempDir::new();
    let d = tmp.path().join("share");
    write(&d.join("a.jpg"), "a\n");
    write(&d.join("b.MOV"), "b\n");
    write(&d.join("c.cr2"), "c\n");
    write(&d.join("d.txt"), "d\n");
    ok(jobs_cmd(&tmp, &["settings", "set", "types.raw", "cr2,nef"]));
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();

    ok(jobs_cmd(&tmp, &["index", "share", "--include-ext", "@images,@raw,@video", "--db", db_s]));
    let mut indexed = indexed_names(&db, &table_for(&d));
    indexed.sort();
    assert_eq!(indexed, ["a.jpg", "b.MOV", "c.cr2"], "groups expanded for indexing");
    let found = ok(jobs_cmd(&tmp, &["search", "share", "--ext", "@raw", "--paths", "--db", db_s]));
    assert_eq!(names(&found.stdout), ["c.cr2"]);
    let unknown = jobs_cmd(&tmp, &["search", "--ext", "@nope", "--db", db_s]);
    assert_eq!(unknown.code, 2);
    assert!(unknown.stderr.contains("no type group @nope"), "{}", unknown.stderr);
}

#[test]
fn log_folder_and_rotation_come_from_settings() {
    let tmp = TempDir::new();
    distinct_files(&tmp.path().join("data"), 2);
    let logs = tmp.path().join("logs");
    ok(jobs_cmd(&tmp, &["settings", "set", "log.folder", logs.to_str().unwrap()]));
    ok(jobs_cmd(&tmp, &["settings", "set", "log.rotate_mb", "1"]));
    let db = tmp.path().join("idx/t.db");
    fs::create_dir_all(&logs).unwrap();
    File::create(logs.join("t.db.log")).unwrap().set_len(1024 * 1024).unwrap();

    ok(jobs_cmd(&tmp, &["index", "data", "--db", db.to_str().unwrap()]));
    assert!(!tmp.path().join("idx/t.db.log").exists(), "not beside the database");
    assert_eq!(fs::metadata(logs.join("t.db.log.1")).unwrap().len(), 1024 * 1024, "rotated at 1 MB");
    assert!(fs::read_to_string(logs.join("t.db.log")).unwrap().contains(" end "));
}

#[test]
fn db_backup_makes_a_usable_copy() {
    let tmp = TempDir::new();
    let d = tmp.path().join("data");
    distinct_files(&d, 3);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "data", "--db", db_s]);

    let copy = tmp.path().join("backups/copy.db");
    let b = db_cmd(tmp.path(), &["backup", copy.to_str().unwrap(), "--db", db_s]);
    assert_eq!(b.code, 0, "{}", b.stderr);
    assert!(b.stdout.starts_with("backed up "), "{}", b.stdout);
    assert_eq!(count(&copy, &format!("SELECT COUNT(*) FROM {}", table_for(&d))), 3);
    let again = db_cmd(tmp.path(), &["backup", copy.to_str().unwrap(), "--db", db_s]);
    assert!(again.code == 1 && again.stderr.contains("already exists"), "never overwrites: {}", again.stderr);
    assert_eq!(log_lines(&db).len(), 2, "a backup changes nothing, so it isn't logged");
}

// ─── Host (5.5.4) ────────────────────────────────────────────────────────────

#[test]
fn host_lists_local_and_network_mounts() {
    let tmp = TempDir::new();
    let out = ok(jobs_cmd(&tmp, &["host"]));
    assert!(out.stdout.contains("jobs store") && out.stdout.contains("store/diskunder.db"), "{}", out.stdout);
    assert!(out.stdout.contains("\nLocal drives\n") && out.stdout.contains("\nNetwork mounts\n"), "{}", out.stdout);
    assert!(out.stdout.contains("MOUNT") && out.stdout.contains("FREE"), "{}", out.stdout);
    assert!(!tmp.path().join("store").exists(), "host doesn't create the store");
}

// ─── Volumes and the database lock (5.6) ─────────────────────────────────────

fn roots_volume(db: &Path, root: &Path) -> (Option<String>, Option<String>) {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT volume, volume_path FROM roots WHERE root = ?1", [root.to_str().unwrap()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
}

#[test]
fn overlapping_paths_count_shared_files_once() {
    let tmp = TempDir::new();
    let d = tmp.path().join("nas");
    write(&d.join("a.bin"), "same\n");
    write(&d.join("proj/b.bin"), "same\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "nas", "--db", db_s]);
    let sub = run_in(tmp.path(), &["index", "nas/proj", "--db", db_s]);
    assert!(sub.stderr.contains("overlaps") && sub.stderr.contains("same volume"), "{}", sub.stderr);

    let (volume, inside) = roots_volume(&db, &d.join("proj"));
    assert!(volume.is_some() && inside.as_deref().is_some_and(|p| p.ends_with("/nas/proj")), "{:?} {:?}", volume, inside);
    let info = db_cmd(tmp.path(), &["info", "--db", db_s]);
    assert!(info.stdout.contains("  volume      ") && info.stdout.contains("  overlaps    "), "{}", info.stdout);

    // b.bin is in both indexes: still two copies (a and b), listed once each
    let r = ok(report(&tmp, &db, &["--duplicates"]));
    assert!(r.stdout.contains("× 2 copies") && r.stdout.matches("b.bin").count() == 1, "{}", r.stdout);
    let found = query(tmp.path(), &["search", "--duplicate", "--format", "csv", "--db", db_s]);
    assert_eq!(found.stdout.lines().filter(|l| l.contains("/proj/b.bin,")).count(), 2, "search lists each index's row: {}", found.stdout);
}

#[test]
fn the_same_file_through_another_mount_is_one_copy() {
    let tmp = TempDir::new();
    write(&tmp.path().join("mac/x.bin"), "film\n");
    write(&tmp.path().join("mac/old/x.bin"), "film\n");
    write(&tmp.path().join("linux/x.bin"), "film\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "mac", "--db", db_s]);
    run_in(tmp.path(), &["index", "linux", "--db", db_s]);
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("× 3 copies"), "three real copies");

    // Make the two paths one share seen from two machines: same volume, same inode,
    // different device numbers
    let conn = Connection::open(&db).unwrap();
    conn.execute("UPDATE roots SET volume = 'smb://nas01/projects', volume_path = '/'", []).unwrap();
    let mac = table_for(&tmp.path().join("mac"));
    let linux = table_for(&tmp.path().join("linux"));
    let ino: i64 = conn.query_row(&format!("SELECT inode FROM {} WHERE filename = 'x.bin' AND directory NOT LIKE '%/old'", mac), [], |r| r.get(0)).unwrap();
    conn.execute(&format!("UPDATE {} SET inode = ?1, device = device + 1", linux), [ino]).unwrap();
    // Runs keep the duplicate summary current; this edit bypasses them, so group the files
    conn.execute("DROP TABLE dup_groups", []).unwrap();
    drop(conn);
    let r = ok(report(&tmp, &db, &["--duplicates"]));
    assert!(r.stdout.contains("× 2 copies"), "linux/x.bin is mac/x.bin: {}", r.stdout);
    assert!(r.stdout.contains("mac/x.bin  (same file, another mount)"), "{}", r.stdout);
}

#[test]
fn schema_4_records_volumes_on_the_next_run() {
    let tmp = TempDir::new();
    let d = tmp.path().join("share");
    write(&d.join("a"), "a\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--db", db_s]);
    // Back to a v5.5 database
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("ALTER TABLE roots DROP COLUMN volume; ALTER TABLE roots DROP COLUMN volume_path;").unwrap();
    conn.pragma_update(None, "user_version", 3).unwrap();
    drop(conn);
    assert_eq!(db_cmd(tmp.path(), &["info", "--db", db_s]).code, 0, "read-only commands cope");
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("Duplicate groups"));
    let upd = run_in(tmp.path(), &["update", "share", "--db", db_s]);
    assert!(upd.stderr.contains("schema 4: added volume columns to the roots table"), "{}", upd.stderr);
    assert_eq!(user_version(&db), 5);
    assert!(roots_volume(&db, &d).0.is_some());
}

#[test]
fn one_run_at_a_time_per_database() {
    use std::os::unix::io::AsRawFd;
    let tmp = TempDir::new();
    write(&tmp.path().join("share/a"), "a\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "share", "--db", db_s]);

    // Another process holds it
    let lock = std::fs::File::create(tmp.path().join("t.db.lock")).unwrap();
    std::fs::write(tmp.path().join("t.db.lock"), "diskunder run nightly — pid 1 on server\n").unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let busy = query(tmp.path(), &["update", "share", "--db", db_s, "--no-tui"]);
    assert_eq!(busy.code, 1);
    assert!(busy.stderr.contains("is in use (diskunder run nightly — pid 1 on server)"), "{}", busy.stderr);
    let vac = db_cmd(tmp.path(), &["vacuum", "--db", db_s]);
    assert!(vac.code == 1 && vac.stderr.contains("is in use"), "{}", vac.stderr);
    assert_eq!(db_cmd(tmp.path(), &["info", "--db", db_s]).code, 0, "reading isn't blocked");

    drop(lock);
    run_in(tmp.path(), &["update", "share", "--db", db_s]);
}

// ─── Reading less (5.7) ──────────────────────────────────────────────────────

/// filename → (has a quick hash, has a checksum) for every row of `root`'s table
fn hash_state(db: &Path, root: &Path) -> std::collections::HashMap<String, (bool, bool)> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(&format!("SELECT filename, quick IS NOT NULL, checksum IS NOT NULL FROM {}", table_for(root)))
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))).unwrap();
    rows.map(Result::unwrap).collect()
}

/// `len` pseudo-random bytes from `seed`
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

fn write_bytes(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

/// Rewrite a file and move its modified time on, so `update` sees the change even within
/// the same second
fn rewrite(p: &Path, data: &[u8]) {
    fs::write(p, data).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    fs::File::options().write(true).open(p).unwrap().set_modified(later).unwrap();
}

#[test]
fn only_files_that_might_be_copies_are_read() {
    let tmp = TempDir::new();
    let d = tmp.path().join("d");
    write(&d.join("unique.txt"), "only one of this size\n");
    write(&d.join("a.txt"), "same\n");
    write(&d.join("b.txt"), "same\n");
    write(&d.join("c.txt"), "diff\n"); // same size as a and b, different bytes
    let big = noise(1, 1_500_000);
    write_bytes(&d.join("big.bin"), &big);
    write_bytes(&d.join("big-copy.bin"), &big);
    let mut other_start = big.clone();
    other_start[0] ^= 0xff;
    write_bytes(&d.join("big-other.bin"), &other_start);
    write_bytes(&d.join("big-unique.bin"), &noise(2, 1_600_000));
    let db = tmp.path().join("t.db");
    let run = run_in(tmp.path(), &["index", "d", "--db", db.to_str().unwrap()]);
    assert!(run.stderr.contains("compared: 5 read in full, 3 quick checks; 3 not read"), "{}", run.stderr);

    let h = hash_state(&db, &d);
    assert_eq!(h["unique.txt"], (false, false), "a size no other file has: not read");
    assert_eq!(h["big-unique.bin"], (false, false));
    assert_eq!(h["a.txt"], (false, true), "small files that might be copies: hashed in full");
    assert_eq!(h["c.txt"], (false, true));
    assert_eq!(h["big.bin"], (true, true), "big: quick hash, then full as the quick hashes match");
    assert_eq!(h["big-copy.bin"], (true, true));
    assert_eq!(h["big-other.bin"], (true, false), "the first 64 KB differ: never read in full");

    let r = ok(report(&tmp, &db, &["--duplicates"]));
    assert!(r.stdout.contains("Duplicate groups       : 2"), "{}", r.stdout);

    // A change makes a file a candidate again
    rewrite(&d.join("big-other.bin"), &big);
    run_in(tmp.path(), &["update", "d", "--db", db.to_str().unwrap()]);
    assert_eq!(hash_state(&db, &d)["big-other.bin"], (true, true));
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("× 3 copies"));
}

#[test]
fn a_copy_in_another_path_is_read_when_reachable() {
    let tmp = TempDir::new();
    write(&tmp.path().join("nas1/report.pdf"), "quarterly numbers\n");
    write(&tmp.path().join("nas1/notes.txt"), "something else\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "nas1", "--db", db_s]);
    assert_eq!(hash_state(&db, &tmp.path().join("nas1"))["report.pdf"], (false, false), "alone: not read");

    // nas2's run reads its own copy and nas1's file, so the group shows straight away
    write(&tmp.path().join("nas2/report-copy.pdf"), "quarterly numbers\n");
    run_in(tmp.path(), &["index", "nas2", "--db", db_s]);
    assert_eq!(hash_state(&db, &tmp.path().join("nas1"))["report.pdf"], (false, true));
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("× 2 copies"));

    // A file changed since its path was indexed isn't read from another path's run
    write(&tmp.path().join("nas1/plan.txt"), "plan A\n");
    run_in(tmp.path(), &["update", "nas1", "--db", db_s]);
    rewrite(&tmp.path().join("nas1/plan.txt"), b"plan B\n");
    write(&tmp.path().join("nas2/plan-copy.txt"), "plan B\n");
    run_in(tmp.path(), &["update", "nas2", "--db", db_s]);
    assert_eq!(hash_state(&db, &tmp.path().join("nas1"))["plan.txt"], (false, false), "stale row left for its own run");
    run_in(tmp.path(), &["update", "nas1", "--db", db_s]);
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("Duplicate groups       : 2"));
}

#[test]
fn hash_all_and_files_hashed_before_5_7() {
    let tmp = TempDir::new();
    let d = tmp.path().join("d");
    write(&d.join("unique.txt"), "only one of this size\n");
    let big = noise(3, 1_200_000);
    write_bytes(&d.join("old.bin"), &big);
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "d", "--db", db_s, "--hash-all"]);
    let h = hash_state(&db, &d);
    assert_eq!(h["unique.txt"], (false, true), "--hash-all reads every file");
    assert_eq!(h["old.bin"], (false, true), "full checksum, no quick hash — like a file hashed before 5.7");

    // A new copy is compared with it in full, as there's no quick hash to compare
    write_bytes(&d.join("new.bin"), &big);
    run_in(tmp.path(), &["update", "d", "--db", db_s]);
    assert_eq!(hash_state(&db, &d)["new.bin"], (true, true));
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("× 2 copies"));

    let dry = dry(&tmp, &["update", "d", "--db", db_s, "--hash-all"]);
    assert!(dry.stdout.contains("(--hash-all)"), "{}", dry.stdout);
}

#[test]
fn the_duplicate_summary_matches_a_full_recount() {
    let tmp = TempDir::new();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    for i in 0..6 {
        write(&a.join(format!("f{i}.txt")), &format!("content {}\n", i % 3)); // 3 groups of 2
    }
    write(&b.join("copy0.txt"), "content 0\n"); // a third copy, in another path
    write(&b.join("own.txt"), "unique, and the only file this size\n");
    let db = tmp.path().join("t.db");
    let db_s = db.to_str().unwrap();
    run_in(tmp.path(), &["index", "a", "--db", db_s]);
    run_in(tmp.path(), &["index", "b", "--db", db_s]);

    // Change one copy, delete another, then drop a whole path
    rewrite(&a.join("f1.txt"), b"content 9\n");
    fs::remove_file(a.join("f2.txt")).unwrap();
    run_in(tmp.path(), &["update", "a", "--db", db_s]);
    let check = |label: &str| {
        let copy = tmp.path().join("recount.db");
        let _ = fs::remove_file(&copy);
        assert_eq!(db_cmd(tmp.path(), &["backup", copy.to_str().unwrap(), "--db", db_s]).code, 0);
        Connection::open(&copy).unwrap().execute("DROP TABLE dup_groups", []).unwrap();
        for args in [
            &["report", "--duplicates"][..],
            &["search", "--duplicate", "--format", "csv"][..],
            &["search", "--duplicate", "--larger-than", "9", "--smaller-than", "11", "--format", "csv"][..],
            &["report", "--duplicates", "--min-size", "10"][..],
        ] {
            let with_summary = query(tmp.path(), &[args, &["--db", db_s]].concat());
            let recount = query(tmp.path(), &[args, &["--db", copy.to_str().unwrap()]].concat());
            assert_eq!(with_summary.stdout, recount.stdout, "{}: {:?}", label, args);
        }
    };
    check("after update");
    let r = ok(report(&tmp, &db, &["--duplicates"]));
    // content 0: f0, f3 and b's copy; f1 changed and f2 was deleted, leaving 1 and 2 single
    assert!(r.stdout.contains("× 3 copies") && r.stdout.contains("Duplicate groups       : 1"), "{}", r.stdout);

    let dropped = db_cmd(tmp.path(), &["drop", "b", "--yes", "--db", db_s]);
    assert_eq!(dropped.code, 0, "{}", dropped.stderr);
    check("after drop");
    assert!(ok(report(&tmp, &db, &["--duplicates"])).stdout.contains("× 2 copies"), "b's copy went with it");
}
