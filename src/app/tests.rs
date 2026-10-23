//! App tests: drive it with key presses and read the screen from ratatui's test backend

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::filters::Overrides;
use crate::settings::Settings;
use crate::run::IndexRun;

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("diskunder-app-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(fs::canonicalize(&dir).unwrap())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn file(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn s(&self, rel: &str) -> String {
        self.path(rel).to_string_lossy().into_owned()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(app: &mut App, keys: &[KeyCode]) {
    for k in keys {
        app.handle_key(key(*k));
    }
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
}

fn screen(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal.draw(|f| draw::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let width = buf.area.width as usize;
    buf.content.chunks(width).map(|row| row.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

/// Let background work finish
fn settle(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while (app.running.is_some() || app.pending.is_some()) && Instant::now() < deadline {
        app.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(app.running.is_none() && app.pending.is_none(), "background work didn't finish");
}

/// A share with a job over it, and a second indexed path with no job
fn setup() -> (Tmp, App) {
    let t = Tmp::new();
    for i in 0..3 {
        t.file(&format!("share/p{}.jpg", i), &format!("photo {}\n", i));
    }
    t.file("share/x.tmp", "tmp\n");
    t.file("other/a.txt", "a\n");
    let db = t.s("idx/index.db");
    let store = t.path("store/diskunder.db");
    let settings = JobSettings {
        upsert: Some(true),
        path: Some(&t.s("share")),
        db: Some(&db),
        threads: Some(2),
        overrides: Overrides { types: Some(crate::filters::TypeFilter { include: None, exclude: vec!["tmp".into()] }), paths: None },
    };
    create_job(&store, "nightly", &settings).unwrap();

    // Index "other" directly, so it appears as a path with no job
    let mut runlog = RunLog::new(&db, None, true);
    let run = IndexRun { upsert: false, typed: t.s("other"), db: db.clone(), threads: 1, overrides: Overrides::default(), no_tui: true };
    let Started { rx, mut state } = start_run(&run, &mut runlog, None).ok().unwrap();
    for msg in rx {
        state.apply(msg);
    }
    finish_run(&state, &mut runlog);

    let app = App::new(store, &db, None, false);
    (t, app)
}

#[test]
fn main_window_lists_jobs_and_paths_with_no_job() {
    let (t, app) = setup();
    let s = screen(&app);
    assert!(s.contains("Jobs  Results  Database  Host  Settings  Help"), "{}", s);
    assert!(s.contains("nightly") && s.contains("– never run"), "{}", s);
    assert!(s.contains("Indexed paths with no job") && s.contains("/other  index "), "long paths shortened from the left:\n{}", s);
    assert!(app.paths.iter().any(|p| p.root.as_ref().is_some_and(|r| r.root == t.s("other"))));
    assert!(s.contains("update · threads 2") && s.contains("filters   exclude tmp"), "details of the selection:\n{}", s);
    assert!(s.contains("/share") && s.contains("index.db"), "long paths keep their ends:\n{}", s);
    assert!(s.contains("$ diskunder run nightly"), "footer shows the equivalent command:\n{}", s);
}

#[test]
fn running_a_job_from_the_app() {
    let (t, mut app) = setup();
    press(&mut app, &[KeyCode::Enter]);
    assert!(app.running.is_some(), "{:?}", app.message);
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("nightly completed in"), "the strip reports the finished run:\n{}", s);
    assert!(s.contains("✓") && s.contains("3 files · 3 written · 0 removed"), "{}", s);

    press(&mut app, &[KeyCode::Char('h')]);
    let s = screen(&app);
    assert!(s.contains("History — nightly") && s.contains("completed"), "{}", s);
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('l')]);
    let s = screen(&app);
    assert!(s.contains("Latest — nightly") && s.contains("found       3") && s.contains("excluded    1 files"), "{}", s);
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('g')]);
    let s = screen(&app);
    assert!(s.contains("Logs — nightly") && s.contains("run nightly [app]"), "the run log names the app run:\n{}", s);
    assert!(fs::read_to_string(t.path("idx/index.db.log")).unwrap().contains(" end "), "logged as usual");
}

#[test]
fn dry_run_of_a_job() {
    let (_t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('y')]);
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("Dry run — nightly") && s.contains("Would hash       3 files"), "{}", s);
    assert!(s.contains("$ diskunder run nightly --dry-run"), "{}", s);
    assert!(job_history(&app.store, "nightly", None).unwrap().is_empty(), "a dry run isn't a run");
}

#[test]
fn test_job_is_created_run_shown_and_removed() {
    let (t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('t')]);
    let s = screen(&app);
    assert!(s.contains(" Test job ") && s.contains("1    Create") && s.contains("4    Remove"), "{}", s);
    assert!(app.command().starts_with("diskunder jobs add test update ") && app.command().contains("--exclude-path .snapshot"));

    press(&mut app, &[KeyCode::Enter]); // Create
    let folder = t.path("store/test");
    assert!(folder.join("share/backup/img1.jpg").exists() && folder.join(".diskunder-test").exists());
    assert!(app.jobs.iter().any(|s| s.job.name == "test"), "{:?}", app.message);
    assert!(matches!(app.dialog, Some(Dialog::TestJob { selected: 1, made: true, ran: false, .. })), "on to Run");
    assert!(screen(&app).contains("1 ✓  Create"));

    press(&mut app, &[KeyCode::Enter]); // Run
    assert!(app.running.is_some(), "{:?}", app.message);
    assert!(matches!(app.dialog, Some(Dialog::TestJob { .. })), "the steps stay up during the run");
    settle(&mut app);
    assert!(app.dialog.is_none(), "the results replace the steps");
    let s = screen(&app);
    assert!(s.contains("Test job — results") && s.contains("d duplicates"), "the results open when it ends:\n{}", s);
    assert!(s.contains("excluded    1 files, 1 folders skipped") && s.contains("duplicates  3 groups"), "the tmp file and the snapshot are left out:\n{}", s);

    press(&mut app, &[KeyCode::Esc]);
    assert!(matches!(app.dialog, Some(Dialog::TestJob { selected: 3, seen: true, .. })), "Esc goes back to the steps");
    assert!(screen(&app).contains("3 ✓  See results"), "{}", screen(&app));
    press(&mut app, &[KeyCode::Char('3')]);
    assert!(screen(&app).contains("Test job — results"), "See results again");
    press(&mut app, &[KeyCode::Char('m')]);
    assert!(matches!(app.dialog, Some(Dialog::TestJob { .. })), "m goes back to the steps too");
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('d')]);
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("img1.jpg") && s.contains("edit.prproj"), "copies show as duplicates:\n{}", s);

    press(&mut app, &[KeyCode::Esc, KeyCode::Char('t')]);
    assert!(matches!(app.dialog, Some(Dialog::TestJob { selected: 3, ran: true, seen: true, .. })), "reopens at Remove");
    press(&mut app, &[KeyCode::Char('4')]); // Remove
    assert!(!folder.exists() && !app.jobs.iter().any(|s| s.job.name == "test"), "{:?}", app.message);
    assert!(app.jobs.iter().any(|s| s.job.name == "nightly"), "other jobs are untouched");
}

#[test]
fn a_job_of_yours_called_test_is_left_alone() {
    let (t, mut app) = setup();
    let settings = JobSettings { upsert: None, path: Some(&t.s("other")), db: None, threads: None, overrides: Overrides::default() };
    create_job(&app.store, "test", &settings).unwrap();
    app.refresh();
    press(&mut app, &[KeyCode::Char('t')]);
    assert!(screen(&app).contains("You already have a job called test"));
    press(&mut app, &[KeyCode::Char('4')]);
    assert!(app.jobs.iter().any(|s| s.job.name == "test") && t.path("other/a.txt").exists());
    assert!(app.message.as_deref().unwrap_or("").contains("rename or remove it"));
}

#[test]
fn host_menu_shows_drives_and_host_info() {
    let (_t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('m'), KeyCode::Left, KeyCode::Left, KeyCode::Left, KeyCode::Enter]); // Host → Drives & mounts
    let s = screen(&app);
    assert!(s.contains("Drives & mounts") && s.contains("Local drives") && s.contains("Network mounts"), "{}", s);
    assert!(s.contains("MOUNT") && s.contains("FREE"), "{}", s);
    assert_eq!(app.command(), "diskunder host");
    let View::Drives { mounts, .. } = &app.view else { panic!("not the drives view") };
    let first = mounts[0].point.clone();
    press(&mut app, &[KeyCode::Char('n')]);
    match &app.dialog {
        Some(Dialog::Form(f)) => assert_eq!(f.fields[2].value, first, "n starts a job for the mount"),
        _ => panic!("no form"),
    }
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('m'), KeyCode::Left, KeyCode::Left, KeyCode::Left, KeyCode::Down, KeyCode::Enter]);
    let s = screen(&app);
    assert!(s.contains("Host info") && s.contains("jobs store") && s.contains("diskunder.db"), "{}", s);
}

#[test]
fn m_goes_back_to_the_menu_you_came_from() {
    let (_t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('m'), KeyCode::Right, KeyCode::Down, KeyCode::Enter]); // Results → History
    assert!(matches!(app.view, View::History { .. }));
    press(&mut app, &[KeyCode::Char('m')]);
    assert!(matches!(app.view, View::Main) && matches!(app.menu, Some(Menu { open: 1, item: 1 })), "back at Results → History");
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('?'), KeyCode::Char('m')]);
    assert!(app.dialog.is_none() && app.menu.is_some(), "dialogs too");
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('/')]);
    type_text(&mut app, "m");
    assert!(matches!(app.view, View::Search(_)) && app.menu.is_none(), "m is a letter while typing");
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('n')]);
    type_text(&mut app, "m");
    assert!(matches!(&app.dialog, Some(Dialog::Form(f)) if f.fields[0].value == "m"), "and in the form");
}

#[test]
fn welcome_lists_the_basics() {
    let t = Tmp::new();
    let app = App::new(t.path("store/diskunder.db"), &t.s("index.db"), None, true);
    let s = screen(&app);
    assert!(s.contains("t      try the test job") && s.contains("q      quit"), "{}", s);
}

#[test]
fn creating_and_editing_jobs_with_the_form() {
    let (t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('n')]);
    type_text(&mut app, "bad name");
    press(&mut app, &[KeyCode::Enter]);
    assert!(screen(&app).contains("job names use letters, digits"), "validation shown in the form");

    for _ in 0.."bad name".len() {
        press(&mut app, &[KeyCode::Backspace]);
    }
    type_text(&mut app, "photos");
    press(&mut app, &[KeyCode::Tab, KeyCode::Char(' '), KeyCode::Tab]); // mode → index, then Path
    type_text(&mut app, &t.s("share"));
    assert!(app.command().starts_with("diskunder jobs add photos index "), "{}", app.command());
    press(&mut app, &[KeyCode::Enter]);
    assert!(app.dialog.is_none(), "{:?}", app.message);
    assert!(app.message.as_deref().unwrap_or("").starts_with("added job photos"), "{:?}", app.message);
    assert!(app.jobs.iter().any(|s| s.job.name == "photos" && !s.job.upsert));

    // Edit: Mode is focused first; Tab ×3 reaches Threads
    press(&mut app, &[KeyCode::Char('e'), KeyCode::Tab, KeyCode::Tab, KeyCode::Tab, KeyCode::Backspace]);
    type_text(&mut app, "6");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.message.as_deref(), Some("updated job photos: threads"));
    assert!(app.jobs.iter().any(|s| s.job.name == "photos" && s.job.threads == 6));
}

#[test]
fn removing_a_job_and_its_index() {
    let (t, mut app) = setup();
    press(&mut app, &[KeyCode::Enter]);
    settle(&mut app);
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(screen(&app).contains("Remove job nightly and its run history?"));
    press(&mut app, &[KeyCode::Char(' '), KeyCode::Char('y')]);
    assert!(app.jobs.is_empty(), "{:?}", app.message);
    assert!(app.message.as_deref().unwrap_or("").contains("removed the index of"), "{:?}", app.message);
    let share = t.s("share");
    assert!(
        !app.paths.iter().any(|p| p.root.as_ref().is_some_and(|r| r.root == share)),
        "the dropped index doesn't reappear as a path with no job"
    );
}

#[test]
fn menus_open_views() {
    let (_t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('m')]);
    assert!(screen(&app).contains("Create…"), "Jobs menu open");
    press(&mut app, &[KeyCode::Right, KeyCode::Down, KeyCode::Enter]); // Results → History
    assert!(matches!(app.view, View::History { .. }));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('m'), KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
    assert!(matches!(app.view, View::Status));
    assert!(screen(&app).contains("Status — all jobs"));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('m'), KeyCode::Left, KeyCode::Left, KeyCode::Left, KeyCode::Left, KeyCode::Enter]); // Database → Info
    settle(&mut app);
    assert!(screen(&app).contains("database: "), "{}", screen(&app));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('?')]);
    assert!(screen(&app).contains("Every screen shows its command line"));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('q')]);
    assert!(app.quit);
}

#[test]
fn selecting_a_path_with_no_job_shows_its_index() {
    let (t, mut app) = setup();
    press(&mut app, &[KeyCode::Down]);
    let s = screen(&app);
    assert!(s.contains("files       1 (2 B)") && s.contains("No job covers this path"), "{}", s);
    assert!(s.contains(&format!("$ diskunder report {} --db", t.s("other"))), "{}", s);
    press(&mut app, &[KeyCode::Char('h')]);
    assert!(app.message.as_deref().unwrap_or("").contains("history is kept for jobs"));
}

#[test]
fn base64_for_the_terminal_clipboard() {
    assert_eq!(base64(b"diskunder run nightly"), "ZGlza3VuZGVyIHJ1biBuaWdodGx5");
    assert_eq!(base64(b"ab"), "YWI=");
}


// ─── Results views (5.5.2) ───────────────────────────────────────────────────

/// The share plus a duplicate and a subfolder, indexed by running the job
fn indexed() -> (Tmp, App) {
    let (t, mut app) = setup();
    t.file("share/copy-of-p0.jpg", "photo 0\n");
    t.file("share/sub/deep/big.mov", &"m".repeat(400));
    press(&mut app, &[KeyCode::Enter]);
    settle(&mut app);
    (t, app)
}

fn wait_for_search(app: &mut App) {
    std::thread::sleep(Duration::from_millis(350));
    app.tick();
}

#[test]
fn search_updates_as_you_type() {
    let (_t, mut app) = indexed();
    press(&mut app, &[KeyCode::Char('/')]);
    let s = screen(&app);
    assert!(s.contains("Search — nightly") && s.contains("5 matches"), "all files (x.tmp excluded by the job) to start with:\n{}", s);
    type_text(&mut app, "p1*");
    wait_for_search(&mut app);
    let s = screen(&app);
    assert!(s.contains("1 matches") && s.contains("p1.jpg"), "{}", s);
    assert!(app.command().contains("--name 'p1*'"), "{}", app.command());

    // Back to all names; sort by size via the choice field
    press(&mut app, &[KeyCode::Backspace, KeyCode::Backspace, KeyCode::Backspace]);
    for _ in 0..8 {
        press(&mut app, &[KeyCode::Tab]);
    }
    press(&mut app, &[KeyCode::Char(' ')]);
    wait_for_search(&mut app);
    assert!(app.command().ends_with(&format!("--sort size --db {}", app.jobs[0].job.db)), "{}", app.command());
    press(&mut app, &[KeyCode::Enter]);
    if let View::Search(x) = &app.view {
        assert!(x.in_results && x.results[0].path.ends_with("big.mov"), "largest first");
    } else {
        panic!("still in search");
    }

    // A bad value shows where the results go, and nothing breaks. (The first Tab leaves the
    // results; then Sort → Name → Types → Larger than.)
    press(&mut app, &[KeyCode::Tab, KeyCode::Tab, KeyCode::Tab, KeyCode::Tab]);
    type_text(&mut app, "lots");
    wait_for_search(&mut app);
    assert!(screen(&app).contains("isn't a size"), "{}", screen(&app));
    press(&mut app, &[KeyCode::Esc]);
    assert!(matches!(app.view, View::Main));
}

#[test]
fn duplicates_view_with_filters() {
    let (_t, mut app) = indexed();
    press(&mut app, &[KeyCode::Char('d')]);
    let s = screen(&app);
    assert!(s.contains("1 groups · 8 B wasted"), "{}", s);
    assert!(s.contains("copy-of-p0.jpg") && s.contains("p0.jpg"), "the selected group's copies:\n{}", s);

    press(&mut app, &[KeyCode::Char('f')]);
    assert!(screen(&app).contains("Duplicate filters"));
    type_text(&mut app, "1K");
    press(&mut app, &[KeyCode::Enter]);
    let s = screen(&app);
    assert!(s.contains("0 groups") && s.contains("min size 1K"), "{}", s);
    assert!(app.command().contains("--duplicates --min-size 1K"), "{}", app.command());

    press(&mut app, &[KeyCode::Char('f'), KeyCode::Char('x'), KeyCode::Esc]);
    assert!(app.command().contains("--min-size 1K --db"), "Esc cancels the edit: {}", app.command());
}

#[test]
fn browse_goes_into_folders_and_back() {
    let (t, mut app) = indexed();
    press(&mut app, &[KeyCode::Char('b')]);
    let s = screen(&app);
    assert!(s.contains("sub/") && s.contains("(files in this folder)"), "{}", s);
    assert!(s.contains("DUPLICATED") && s.contains("16 B"), "the jpg and its copy count as duplicated:\n{}", s);
    // sub/ holds the 400-byte mov, so it's first
    press(&mut app, &[KeyCode::Enter]);
    assert!(app.command().contains(&format!("report {} --by-directory", t.s("share/sub"))), "{}", app.command());
    assert!(screen(&app).contains("deep/"));
    press(&mut app, &[KeyCode::Backspace]);
    assert!(app.command().contains(&format!("report {} --by-directory", t.s("share"))), "{}", app.command());
    press(&mut app, &[KeyCode::Esc]);
    assert!(matches!(app.view, View::Main));
}

#[test]
fn reports_menu_shows_the_report() {
    let (_t, mut app) = indexed();
    press(&mut app, &[KeyCode::Char('p')]);
    assert!(screen(&app).contains("Space by folder"));
    press(&mut app, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]); // Space by type
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("Space by type — nightly") && s.contains("TYPE") && s.contains("jpg"), "{}", s);
    assert!(app.command().contains("--by-type --db"), "{}", app.command());

    press(&mut app, &[KeyCode::Esc, KeyCode::Char('p'), KeyCode::Enter]); // Overview
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("Overview — nightly") && s.contains("Duplicates             1 group"), "{}", s);
}


// ─── Database and Settings menus (5.5.3) ─────────────────────────────────────

fn open_database_menu(app: &mut App, item: usize) {
    press(app, &[KeyCode::Char('m'), KeyCode::Right, KeyCode::Right]);
    for _ in 0..item {
        press(app, &[KeyCode::Down]);
    }
    press(app, &[KeyCode::Enter]);
}

#[test]
fn database_menu_info_check_vacuum_and_backup() {
    let (t, mut app) = indexed();
    open_database_menu(&mut app, 0);
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("database: ") && s.contains("/share") && s.contains("/other"), "both indexes:\n{}", s);
    assert_eq!(app.command(), format!("diskunder db info --db {}", app.db));

    press(&mut app, &[KeyCode::Esc]);
    open_database_menu(&mut app, 1);
    settle(&mut app);
    assert!(screen(&app).contains("integrity: ok"), "{}", screen(&app));

    press(&mut app, &[KeyCode::Esc]);
    open_database_menu(&mut app, 2);
    assert!(screen(&app).contains("Compact the database file?"));
    press(&mut app, &[KeyCode::Char('y')]);
    settle(&mut app);
    assert!(screen(&app).contains("vacuumed "), "{}", screen(&app));
    let log = fs::read_to_string(format!("{}.log", app.db)).unwrap();
    assert!(log.contains(&format!("start  db vacuum --db {} [app]", app.db)), "the app names what it did:\n{}", log);

    press(&mut app, &[KeyCode::Esc]);
    open_database_menu(&mut app, 5);
    if let Some(Dialog::Input { value, .. }) = &app.dialog {
        assert!(value.starts_with(&format!("{}.backup-", app.db)), "{}", value);
    } else {
        panic!("backup asks where to");
    }
    press(&mut app, &[KeyCode::Char('u')]);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, &t.s("backups/copy.db"));
    assert_eq!(app.command(), format!("diskunder db backup {} --db {}", t.s("backups/copy.db"), app.db));
    press(&mut app, &[KeyCode::Enter]);
    settle(&mut app);
    assert!(screen(&app).contains("backed up "), "{}", screen(&app));
    let copy = Connection::open(t.path("backups/copy.db")).unwrap();
    let tables: i64 = copy.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'files_%' AND type='table'", [], |r| r.get(0)).unwrap();
    assert_eq!(tables, 2, "a full copy, both indexes");
}

#[test]
fn database_menu_drops_an_index_and_warns_about_jobs() {
    let (t, mut app) = indexed();
    open_database_menu(&mut app, 3);
    let s = screen(&app);
    assert!(s.contains("(job nightly)") && s.contains("/other"), "{}", s);
    // The job's index first (alphabetical), then other — choose the job's to see the warning
    let job_row = match &app.dialog {
        Some(Dialog::DropIndex { items, .. }) => items.iter().position(|i| i.job.is_some()).unwrap(),
        _ => panic!("drop chooser"),
    };
    for _ in 0..job_row {
        press(&mut app, &[KeyCode::Down]);
    }
    press(&mut app, &[KeyCode::Enter]);
    assert!(screen(&app).contains("Job nightly uses it"), "{}", screen(&app));
    press(&mut app, &[KeyCode::Esc]);

    open_database_menu(&mut app, 3);
    let other_row = match &app.dialog {
        Some(Dialog::DropIndex { items, .. }) => items.iter().position(|i| i.label == t.s("other")).unwrap(),
        _ => panic!("drop chooser"),
    };
    for _ in 0..other_row {
        press(&mut app, &[KeyCode::Down]);
    }
    press(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
    assert!(app.message.as_deref().unwrap_or("").starts_with("removed the index of"), "{:?}", app.message);
    assert!(app.paths.is_empty(), "other no longer listed");
}

#[test]
fn database_menu_opens_another_database() {
    let (t, mut app) = setup();
    open_database_menu(&mut app, 4);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, &t.s("elsewhere.db"));
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.db, t.s("elsewhere.db"));
    assert!(app.paths.is_empty());
    assert!(app.message.as_deref().unwrap_or("").contains("nothing indexed there yet"));
    assert!(screen(&app).contains("elsewhere.db"), "the menu bar shows the open database");
}

fn settings_row(app: &App, key: &str) -> usize {
    match &app.view {
        View::Settings(v) => v.rows.iter().position(|r| matches!(r, settings_view::RowKind::Setting(k) if *k == key)).unwrap(),
        _ => panic!("not in settings"),
    }
}

fn select_setting(app: &mut App, key: &str) {
    let i = settings_row(app, key);
    if let View::Settings(v) = &mut app.view {
        v.selected = i;
    }
}

#[test]
fn settings_are_edited_in_place_and_used() {
    let (_t, mut app) = setup();
    app.act(Action::Settings(1));
    assert!(screen(&app).contains("Defaults for new jobs") && screen(&app).contains("defaults.threads"));

    select_setting(&mut app, "defaults.threads");
    press(&mut app, &[KeyCode::Enter]);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, "8");
    assert_eq!(app.command(), "diskunder settings set defaults.threads 8");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.message.as_deref(), Some("defaults.threads = 8"));
    assert_eq!(Settings::load(&app.store).get("defaults.threads"), "8");

    select_setting(&mut app, "defaults.threads");
    press(&mut app, &[KeyCode::Enter, KeyCode::Char('x'), KeyCode::Enter]);
    assert!(app.message.as_deref().unwrap_or("").contains("whole number"), "bad values are refused: {:?}", app.message);
    press(&mut app, &[KeyCode::Esc]);

    // Turning off dry run first asks; anything but y keeps it
    select_setting(&mut app, "deletion.dry_run_default");
    press(&mut app, &[KeyCode::Enter]);
    assert!(screen(&app).contains("turn off dry run first?"));
    press(&mut app, &[KeyCode::Char('n')]);
    assert!(app.settings.yes("deletion.dry_run_default"));
    press(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
    assert!(!app.settings.yes("deletion.dry_run_default"));
    press(&mut app, &[KeyCode::Char('x')]);
    assert!(app.settings.yes("deletion.dry_run_default"), "x puts it back to the default");

    // A new type group, then the defaults in the New job form
    let add = match &app.view {
        View::Settings(v) => v.rows.iter().position(|r| *r == settings_view::RowKind::AddGroup).unwrap(),
        _ => unreachable!(),
    };
    if let View::Settings(v) = &mut app.view {
        v.selected = add;
    }
    press(&mut app, &[KeyCode::Enter]);
    type_text(&mut app, "stills: jpg,png");
    press(&mut app, &[KeyCode::Enter]);
    assert_eq!(app.settings.expand_types("@stills").unwrap(), "jpg,png");
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('n')]);
    let Some(Dialog::Form(form)) = &app.dialog else { panic!("form") };
    assert_eq!(form.fields[4].value, "8", "threads default prefilled");
}

#[test]
fn type_groups_work_in_search_and_the_job_form() {
    let (t, mut app) = indexed();
    press(&mut app, &[KeyCode::Char('/'), KeyCode::Tab]);
    type_text(&mut app, "@video");
    std::thread::sleep(Duration::from_millis(350));
    app.tick();
    assert!(screen(&app).contains("1 matches") && screen(&app).contains("big.mov"), "{}", screen(&app));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('n')]);
    type_text(&mut app, "clips");
    press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
    type_text(&mut app, &t.s("share"));
    for _ in 0..3 {
        press(&mut app, &[KeyCode::Tab]);
    }
    type_text(&mut app, "@video");
    press(&mut app, &[KeyCode::Enter]);
    let job = app.jobs.iter().find(|s| s.job.name == "clips").expect("created");
    assert!(job.job.filter.types.include.as_ref().unwrap().contains(&"mov".to_string()), "the group was expanded");
}

#[test]
fn the_app_selects_the_last_run_job() {
    let (t, mut app) = setup();
    let settings = JobSettings { upsert: Some(false), path: Some(&t.s("other")), db: Some(&t.s("idx/second.db")), threads: None, overrides: Overrides::default() };
    create_job(&app.store, "a-first", &settings).unwrap();
    app.refresh();
    let i = app.jobs.iter().position(|s| s.job.name == "nightly").unwrap();
    app.selected = i;
    press(&mut app, &[KeyCode::Enter]);
    settle(&mut app);
    let reopened = App::new(app.store.clone(), &app.db, None, true);
    assert!(matches!(reopened.selected(), Selected::Job(s) if s.job.name == "nightly"), "a-first sorts first, but nightly ran last");
}
