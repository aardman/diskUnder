// End-to-end tests: run the real binary in plain mode against throwaway directories,
// then check its summary output and the SQLite database it wrote.

use rusqlite::Connection;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_diskunder");

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
    let out = Command::new(BIN)
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
    let not_written = 1502 - 1 - s.written;
    assert!(not_written >= 1 && not_written <= 1000, "one batch lost: {}", not_written);
    assert_eq!(
        s.errors,
        format!("{} errors (1 unreadable, {} not written, 1 traversal/db)", not_written + 2, not_written)
    );

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
    let out = Command::new(BIN)
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

    let out = Command::new(BIN)
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
        let out = Command::new(BIN).args(args).output().unwrap();
        assert!(!out.status.success(), "{:?} should fail", args);
    }
    let help = Command::new(BIN).arg("--help").output().unwrap();
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
    let out = Command::new(BIN)
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
    assert_eq!(user_version(&db), 1);
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
    assert_eq!(user_version(&db), 1);
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
    let out = Command::new(BIN).args(args).current_dir(cwd).output().unwrap();
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
