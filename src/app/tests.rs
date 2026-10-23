//! App tests: drive it with key presses and read the screen from ratatui's test backend

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::filters::Overrides;
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
    while (app.running.is_some() || app.testing.is_some()) && Instant::now() < deadline {
        app.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(app.running.is_none() && app.testing.is_none(), "background work didn't finish");
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
    assert!(s.contains("Jobs  Results  Database  Settings  Help"), "{}", s);
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
fn testing_a_job_shows_the_dry_run() {
    let (_t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('t')]);
    settle(&mut app);
    let s = screen(&app);
    assert!(s.contains("Test — nightly (dry run)") && s.contains("Would hash       3 files"), "{}", s);
    assert!(s.contains("$ diskunder run nightly --dry-run"), "{}", s);
    assert!(job_history(&app.store, "nightly", None).unwrap().is_empty(), "a test isn't a run");
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
fn menus_open_views_and_say_what_comes_later() {
    let (_t, mut app) = setup();
    press(&mut app, &[KeyCode::Char('m')]);
    assert!(screen(&app).contains("Create…"), "Jobs menu open");
    press(&mut app, &[KeyCode::Right, KeyCode::Down, KeyCode::Enter]); // Results → History
    assert!(matches!(app.view, View::History { .. }));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('m'), KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
    assert!(matches!(app.view, View::Status));
    assert!(screen(&app).contains("Status — all jobs"));
    press(&mut app, &[KeyCode::Esc, KeyCode::Char('m'), KeyCode::Left, KeyCode::Left, KeyCode::Left, KeyCode::Enter]);
    assert_eq!(app.message.as_deref(), Some("the Database and Settings menus come in 5.5.3"));
    press(&mut app, &[KeyCode::Char('?')]);
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
