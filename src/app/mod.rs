//! The interactive app: `diskunder` with no subcommand, in a terminal. A front end to the
//! jobs and runs the command line already has — every screen shows its equivalent command.

mod draw;
mod form;
mod results;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use crate::db::{file_tables, load_roots, table_duplicates, RootRow};
use crate::dryrun::dry_run_report;
use crate::jobs::{
    begin_run, create_job, delete_job, job_history, job_run_ids, job_summaries, run_errors, ActiveRun, Job,
    JobSettings, JobSummary, RunRow,
};
use crate::maintenance::drop_index;
use crate::query::absolute;
use crate::run::{finish_run, start_run, Started};
use crate::runlog::RunLog;
use form::{Form, FormKey};
use results::{Browse, Dups, Outcome, Search, Target};

const TICK: Duration = Duration::from_millis(100);
const RECENT_RUNS: usize = 4; // for the details pane

/// An indexed path in the open database that no job covers
pub(crate) struct PathEntry {
    pub(crate) table: String,
    pub(crate) root: Option<RootRow>,
    pub(crate) details: Option<PathDetails>, // filled in when first selected
}

pub(crate) struct PathDetails {
    pub(crate) files: i64,
    pub(crate) bytes: i64,
    pub(crate) groups: i64,
    pub(crate) wasted: i64,
}

/// What fills the area below the menu bar
pub(crate) enum View {
    Main,
    /// Every job and its last run
    Status,
    /// A scrollable report: Latest, Errors, or a Test (dry run)
    Text { title: String, lines: Vec<String>, scroll: usize, command: String },
    History { job: String, runs: Vec<RunRow>, selected: usize },
    Logs { job: Option<String>, db: String, all: bool, lines: Vec<String>, scroll: usize },
    Search(Box<Search>),
    Duplicates(Box<Dups>),
    Browse(Box<Browse>),
}

/// The report sections the Reports menu offers: (label, `report` options)
pub(crate) const REPORTS: [(&str, &str); 6] = [
    ("Overview", ""),
    ("Largest files", "--top-files"),
    ("Space by type", "--by-type"),
    ("Space by folder", "--by-directory"),
    ("Not modified in 1 year", "--stale 1y"),
    ("Duplicates with summary", "--duplicates"),
];

pub(crate) enum Dialog {
    Form(Form),
    RemoveJob { name: String, drop_index: bool },
    QuitWhileRunning,
    Help,
    Reports { selected: usize },
    DupFilters { saved: Vec<String> }, // values to restore if the edit is cancelled
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Action {
    NewJob,
    Run,
    Test,
    Edit,
    Status,
    Remove,
    Latest,
    History,
    Errors,
    Logs,
    Search,
    Duplicates,
    Reports,
    Browse,
    Later, // Database and Settings menus: 5.5.3
    Help,
    Quit,
}

/// The menu bar: (title, [(item, key hint, action)])
pub(crate) const MENUS: [(&str, &[(&str, &str, Action)]); 5] = [
    (
        "Jobs",
        &[
            ("Create…", "n", Action::NewJob),
            ("Run", "⏎", Action::Run),
            ("Test", "t", Action::Test),
            ("Edit…", "e", Action::Edit),
            ("Status", "s", Action::Status),
            ("Remove…", "x", Action::Remove),
        ],
    ),
    (
        "Results",
        &[
            ("Latest", "l", Action::Latest),
            ("History", "h", Action::History),
            ("Errors", "o", Action::Errors),
            ("Logs", "g", Action::Logs),
            ("Search…", "/", Action::Search),
            ("Duplicates", "d", Action::Duplicates),
            ("Reports…", "p", Action::Reports),
            ("Browse", "b", Action::Browse),
        ],
    ),
    ("Database", &[("Coming in 5.5.3 — use `diskunder db` for now", "", Action::Later)]),
    ("Settings", &[("Coming in 5.5.3", "", Action::Later)]),
    ("Help", &[("Keys", "?", Action::Help), ("Quit", "q", Action::Quit)]),
];

pub(crate) struct Menu {
    pub(crate) open: usize,
    pub(crate) item: usize,
}

/// A job run in progress in the bottom strip
pub(crate) struct RunningJob {
    pub(crate) name: String,
    pub(crate) started: Started,
    runlog: RunLog,
    active: ActiveRun,
}

pub(crate) struct App {
    pub(crate) store: PathBuf,
    pub(crate) db: String, // the open index database (for paths with no job)
    log: Option<String>,
    no_log: bool,
    pub(crate) jobs: Vec<JobSummary>,
    pub(crate) recent: HashMap<String, Vec<RunRow>>, // job → latest runs, for the details pane
    pub(crate) paths: Vec<PathEntry>,
    pub(crate) selected: usize, // over jobs, then paths
    pub(crate) view: View,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) menu: Option<Menu>,
    pub(crate) running: Option<RunningJob>,
    pub(crate) finished: Option<String>, // the strip's last line once a run ends
    pending: Option<Pending>, // a test or report working in the background
    pub(crate) message: Option<String>, // one-line feedback in the footer
    pub(crate) quit: bool,
}

/// Background work whose result becomes a text view
pub(crate) struct Pending {
    title: String,
    command: String,
    rx: Receiver<Result<(String, Vec<String>), String>>,
}

pub(crate) enum Selected<'a> {
    Job(&'a JobSummary),
    Path(&'a PathEntry),
    Nothing,
}

impl App {
    pub(crate) fn new(store: PathBuf, db: &str, log: Option<String>, no_log: bool) -> Self {
        let mut app = App {
            store,
            db: absolute(db),
            log,
            no_log,
            jobs: Vec::new(),
            recent: HashMap::new(),
            paths: Vec::new(),
            selected: 0,
            view: View::Main,
            dialog: None,
            menu: None,
            running: None,
            finished: None,
            pending: None,
            message: None,
            quit: false,
        };
        app.refresh();
        app
    }

    /// Reload jobs and the open database's indexed paths
    pub(crate) fn refresh(&mut self) {
        match job_summaries(&self.store) {
            Ok(jobs) => self.jobs = jobs,
            Err(e) => self.message = Some(format!("can't read jobs: {}", e)),
        }
        self.recent = self
            .jobs
            .iter()
            .filter_map(|s| job_history(&self.store, &s.job.name, Some(RECENT_RUNS)).ok().map(|r| (s.job.name.clone(), r)))
            .collect();
        self.paths = self.unjobbed_paths();
        let n = self.jobs.len() + self.paths.len();
        if n > 0 && self.selected >= n {
            self.selected = n - 1;
        }
    }

    fn unjobbed_paths(&self) -> Vec<PathEntry> {
        if !Path::new(&self.db).exists() {
            return Vec::new();
        }
        let Ok(conn) = Connection::open(&self.db) else { return Vec::new() };
        let roots = load_roots(&conn).unwrap_or_default();
        let mut tables = file_tables(&conn).unwrap_or_default();
        tables.sort_by_key(|t| (roots.get(t).is_none(), roots.get(t).map(|r| r.root.clone())));
        tables
            .into_iter()
            .filter(|t| {
                let root = roots.get(t).map(|r| r.root.as_str());
                !self.jobs.iter().any(|s| s.job.db == self.db && Some(s.job.path.as_str()) == root)
            })
            .map(|t| {
                let root = roots.get(&t).map(|r| RootRow {
                    root: r.root.clone(),
                    first_run: r.first_run.clone(),
                    last_started: r.last_started.clone(),
                    last_finished: r.last_finished.clone(),
                    last_mode: r.last_mode.clone(),
                    filter: r.filter.clone(),
                });
                PathEntry { table: t, root, details: None }
            })
            .collect()
    }

    pub(crate) fn selected(&self) -> Selected<'_> {
        if let Some(j) = self.jobs.get(self.selected) {
            Selected::Job(j)
        } else if let Some(p) = self.paths.get(self.selected - self.jobs.len()) {
            Selected::Path(p)
        } else {
            Selected::Nothing
        }
    }

    fn selected_job(&self) -> Option<Job> {
        match self.selected() {
            Selected::Job(s) => Some(s.job.clone()),
            _ => None,
        }
    }

    /// Counts for the selected path, computed once (they can take a moment on big indexes)
    fn load_path_details(&mut self) {
        let i = match self.selected.checked_sub(self.jobs.len()) {
            Some(i) if i < self.paths.len() && self.paths[i].details.is_none() => i,
            _ => return,
        };
        let Ok(conn) = Connection::open(&self.db) else { return };
        let t = self.paths[i].table.clone();
        let (files, bytes): (i64, i64) = conn
            .query_row(&format!("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM {t}"), [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((0, 0));
        let (groups, wasted) = table_duplicates(&conn, &t).unwrap_or((0, 0));
        self.paths[i].details = Some(PathDetails { files, bytes, groups, wasted });
    }

    /// The command line equivalent to what's on screen
    pub(crate) fn command(&self) -> String {
        if let Some(Dialog::Form(f)) = &self.dialog {
            return f.command();
        }
        match &self.view {
            View::Status => "diskunder jobs list".into(),
            View::Text { command, .. } => command.clone(),
            View::History { job, .. } => format!("diskunder jobs show {}", job),
            View::Logs { db, .. } => format!("less {}.log", db),
            View::Search(x) => x.command(),
            View::Duplicates(x) => x.command(),
            View::Browse(x) => x.command(),
            View::Main => match self.selected() {
                Selected::Job(s) => format!("diskunder run {}", s.job.name),
                Selected::Path(p) => match &p.root {
                    Some(r) => format!("diskunder report {} --db {}", r.root, self.db),
                    None => format!("diskunder db info --db {}", self.db),
                },
                Selected::Nothing => "diskunder jobs add NAME update PATH --db FILE".into(),
            },
        }
    }

    // ─── Background work ─────────────────────────────────────────────────────

    /// Apply progress from a run or test in progress; finish them when they end
    pub(crate) fn tick(&mut self) {
        if let Some(run) = &mut self.running {
            loop {
                match run.started.rx.try_recv() {
                    Ok(msg) => {
                        run.runlog.observe(&msg);
                        run.started.state.apply(msg);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if !run.started.state.done {
                            run.started.state.fatal = Some("the indexing thread stopped unexpectedly".into());
                            run.started.state.done = true;
                        }
                        break;
                    }
                }
            }
            if run.started.state.done {
                self.finish_running();
            }
        }
        if let Some(p) = &self.pending {
            if let Ok(result) = p.rx.try_recv() {
                let p = self.pending.take().unwrap();
                self.message = None; // "working…" is done
                match result {
                    Ok((text, notes)) => {
                        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
                        if !notes.is_empty() {
                            lines.push(String::new());
                            lines.extend(notes.iter().map(|n| n.to_string()));
                        }
                        self.view = View::Text { title: p.title, lines, scroll: 0, command: p.command };
                    }
                    Err(e) => self.message = Some(format!("{}: {}", p.title, e)),
                }
            }
        }
        if let View::Search(s) = &mut self.view {
            s.tick();
        }
    }

    /// Run `work` in the background; its text becomes a view titled `title`
    fn spawn_text(
        &mut self,
        title: String,
        command: String,
        work: impl FnOnce() -> Result<(String, Vec<String>), String> + Send + 'static,
    ) {
        if self.pending.is_some() {
            self.message = Some("still working on the last request".into());
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.message = Some(format!("{} — working…", title));
        self.pending = Some(Pending { title, command, rx });
    }

    /// The index the selection points at, for the Results views
    fn target(&mut self) -> Option<Target> {
        let t = match self.selected() {
            Selected::Job(s) => Some(Target { label: s.job.name.clone(), db: s.job.db.clone(), path: s.job.path.clone() }),
            Selected::Path(p) => p.root.as_ref().map(|r| Target { label: r.root.clone(), db: self.db.clone(), path: r.root.clone() }),
            Selected::Nothing => None,
        };
        if t.is_none() {
            self.message = Some("select a job or an indexed path first".into());
        }
        t
    }

    /// Record the run that just ended (or is being stopped) in the job's history
    fn finish_running(&mut self) {
        let Some(mut run) = self.running.take() else { return };
        let outcome = finish_run(&run.started.state, &mut run.runlog);
        let took = crate::jobs::fmt_secs(run.runlog.duration_secs() as i64);
        if let Err(e) = run.active.finish(&outcome, &run.runlog) {
            self.message = Some(format!("couldn't record the run: {}", e));
        }
        self.finished = Some(format!("{} {} in {} — {}", run.name, outcome.outcome, took, outcome.summary));
        if let Some(reason) = run.runlog.failure() {
            self.message = Some(reason.to_string());
        }
        self.refresh();
    }

    fn run_job(&mut self) {
        let Some(job) = self.selected_job() else {
            self.message = Some("select a job to run".into());
            return;
        };
        if let Some(r) = &self.running {
            self.message = Some(format!("{} is still running — one run at a time", r.name));
            return;
        }
        let mut runlog = RunLog::new(&job.db, self.log.as_deref(), self.no_log);
        let (active, index_run) = match begin_run(&self.store, &job.name, None, true, &runlog) {
            Ok(x) => x,
            Err(e) => {
                self.message = Some(e);
                return;
            }
        };
        match start_run(&index_run, &mut runlog, Some(format!("run {} [app]", job.name))) {
            Ok(started) => {
                self.finished = None;
                self.running = Some(RunningJob { name: job.name.clone(), started, runlog, active });
                self.refresh();
            }
            Err(failed) => {
                // Recorded like any other failed run, with the reason
                let _ = active.finish(&failed, &runlog);
                self.finished = Some(format!("{} failed — {}", job.name, failed.summary));
                self.refresh();
            }
        }
    }

    fn test_job(&mut self) {
        let Some(job) = self.selected_job() else {
            self.message = Some("select a job to test".into());
            return;
        };
        let root = match fs::canonicalize(&job.path) {
            Ok(r) => r.to_string_lossy().into_owned(),
            Err(e) => {
                self.message = Some(format!("path not found: {} ({})", job.path, e));
                return;
            }
        };
        let title = format!("Test — {} (dry run)", job.name);
        let command = format!("diskunder run {} --dry-run", job.name);
        self.spawn_text(title, command, move || {
            let (report, warnings) = dry_run_report(&job.db, &job.path, &root, job.upsert, &job.overrides())?;
            Ok((report, warnings.iter().map(|w| format!("warn: {}", w)).collect()))
        });
    }

    // ─── Views ───────────────────────────────────────────────────────────────

    fn show_latest(&mut self) {
        self.load_path_details();
        let (title, lines, command) = match self.selected() {
            Selected::Job(s) => {
                let name = s.job.name.clone();
                let lines = match &s.last {
                    None => vec!["never run — press ⏎ in the main window to run it".to_string()],
                    Some(r) => draw::run_lines(r, &run_errors(&self.store, r.id).unwrap_or_default(), s.running),
                };
                (format!("Latest — {}", name), lines, format!("diskunder jobs show {}", name))
            }
            Selected::Path(p) => (
                format!("Latest — {}", p.root.as_ref().map_or(p.table.as_str(), |r| r.root.as_str())),
                draw::path_lines(p),
                format!("diskunder db info --db {}", self.db),
            ),
            Selected::Nothing => return,
        };
        self.view = View::Text { title, lines, scroll: 0, command };
    }

    fn show_history(&mut self) {
        match self.selected_job() {
            Some(job) => match job_history(&self.store, &job.name, None) {
                Ok(runs) => self.view = View::History { job: job.name, runs, selected: 0 },
                Err(e) => self.message = Some(e),
            },
            None => self.message = Some("history is kept for jobs — this path only records its last run".into()),
        }
    }

    fn show_errors(&mut self, run: Option<&RunRow>, job: &str) {
        let Some(r) = run else {
            self.message = Some(format!("{} has no runs yet", job));
            return;
        };
        let errors = run_errors(&self.store, r.id).unwrap_or_default();
        let mut lines: Vec<String> = if errors.is_empty() {
            vec!["no errors".into()]
        } else {
            errors.iter().map(|e| e.to_string()).collect()
        };
        if let Some(d) = r.errors_dropped.filter(|d| *d > 0) {
            lines.push(format!("… and {} more (not kept)", d));
        }
        if let Some(n) = r.errors_unreadable.filter(|n| *n > 0) {
            lines.push(String::new());
            lines.push(format!("plus {} files that couldn't be read — counted, not listed", n));
        }
        self.view = View::Text {
            title: format!("Errors — {} run {} ({})", job, r.run_id, crate::fmt::fmt_local(&r.started)),
            lines,
            scroll: 0,
            command: format!("diskunder jobs show {}", job),
        };
    }

    fn show_logs(&mut self) {
        let (job, db) = match self.selected() {
            Selected::Job(s) => (Some(s.job.name.clone()), s.job.db.clone()),
            Selected::Path(_) => (None, self.db.clone()),
            Selected::Nothing => return,
        };
        let lines = self.log_lines(job.as_deref(), &db, false);
        let scroll = lines.len().saturating_sub(1);
        self.view = View::Logs { job, db, all: false, lines, scroll };
    }

    /// The run log beside `db`; just this job's runs unless `all`
    fn log_lines(&self, job: Option<&str>, db: &str, all: bool) -> Vec<String> {
        let mut text = fs::read_to_string(format!("{}.log.1", db)).unwrap_or_default();
        text.push_str(&fs::read_to_string(format!("{}.log", db)).unwrap_or_default());
        let ids: Vec<String> = match (job, all) {
            (Some(name), false) => job_run_ids(&self.store, name).unwrap_or_default(),
            _ => Vec::new(),
        };
        let only_job = job.is_some() && !all;
        let lines: Vec<String> = text
            .lines()
            .filter(|l| !only_job || ids.iter().any(|id| l.contains(&format!(" run={} ", id))))
            .map(str::to_string)
            .collect();
        if lines.is_empty() {
            vec![format!("nothing logged yet in {}.log", db)]
        } else {
            lines
        }
    }

    // ─── Keys ────────────────────────────────────────────────────────────────

    pub(crate) fn act(&mut self, action: Action) {
        self.menu = None;
        self.view = View::Main;
        match action {
            Action::NewJob => self.dialog = Some(Dialog::Form(Form::new_job(&self.db))),
            Action::Run => self.run_job(),
            Action::Test => self.test_job(),
            Action::Edit => match self.selected_job() {
                Some(job) => self.dialog = Some(Dialog::Form(Form::edit_job(&job))),
                None => self.message = Some("select a job to edit".into()),
            },
            Action::Status => self.view = View::Status,
            Action::Remove => match self.selected_job() {
                Some(job) => self.dialog = Some(Dialog::RemoveJob { name: job.name, drop_index: false }),
                None => self.message = Some("select a job to remove".into()),
            },
            Action::Latest => self.show_latest(),
            Action::History => self.show_history(),
            Action::Errors => {
                if let Selected::Job(s) = self.selected() {
                    let name = s.job.name.clone();
                    let last = job_history(&self.store, &name, Some(1)).ok().and_then(|mut r| r.pop());
                    self.show_errors(last.as_ref(), &name);
                } else {
                    self.message = Some("errors are kept for job runs".into());
                }
            }
            Action::Logs => self.show_logs(),
            Action::Search => {
                if let Some(t) = self.target() {
                    self.view = View::Search(Box::new(Search::new(t)));
                }
            }
            Action::Duplicates => {
                if let Some(t) = self.target() {
                    self.view = View::Duplicates(Box::new(Dups::new(t)));
                }
            }
            Action::Browse => {
                if let Some(t) = self.target() {
                    self.view = View::Browse(Box::new(Browse::new(t)));
                }
            }
            Action::Reports => {
                if self.target().is_some() {
                    self.dialog = Some(Dialog::Reports { selected: 0 });
                }
            }
            Action::Later => self.message = Some("the Database and Settings menus come in 5.5.3".into()),
            Action::Help => self.dialog = Some(Dialog::Help),
            Action::Quit => self.request_quit(),
        }
    }

    fn request_quit(&mut self) {
        if self.running.is_some() {
            self.dialog = Some(Dialog::QuitWhileRunning);
        } else {
            self.quit = true;
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.message = None;
        if let Some(dialog) = self.dialog.take() {
            self.dialog_key(dialog, key);
            return;
        }
        if let Some(menu) = self.menu.take() {
            self.menu_key(menu, key);
            return;
        }
        if !matches!(self.view, View::Main) {
            self.view_key(key);
            return;
        }
        let n = self.jobs.len() + self.paths.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < n {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => match self.selected() {
                Selected::Job(_) => self.act(Action::Run),
                Selected::Path(_) => self.act(Action::Latest),
                Selected::Nothing => self.act(Action::NewJob),
            },
            KeyCode::Char('t') => self.act(Action::Test),
            KeyCode::Char('n') => self.act(Action::NewJob),
            KeyCode::Char('e') => self.act(Action::Edit),
            KeyCode::Char('x') | KeyCode::Delete => self.act(Action::Remove),
            KeyCode::Char('s') => self.act(Action::Status),
            KeyCode::Char('l') => self.act(Action::Latest),
            KeyCode::Char('h') => self.act(Action::History),
            KeyCode::Char('o') => self.act(Action::Errors),
            KeyCode::Char('g') => self.act(Action::Logs),
            KeyCode::Char('/') => self.act(Action::Search),
            KeyCode::Char('d') => self.act(Action::Duplicates),
            KeyCode::Char('p') => self.act(Action::Reports),
            KeyCode::Char('b') => self.act(Action::Browse),
            KeyCode::Char('r') => self.menu = Some(Menu { open: 1, item: 0 }),
            KeyCode::Char('m') | KeyCode::F(10) => self.menu = Some(Menu { open: 0, item: 0 }),
            KeyCode::Char('?') => self.act(Action::Help),
            KeyCode::Char('c') => self.copy_command(),
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
        if matches!(self.selected(), Selected::Path(_)) {
            self.load_path_details();
        }
    }

    fn menu_key(&mut self, mut menu: Menu, key: KeyEvent) {
        let items = MENUS[menu.open].1.len();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Left => {
                menu.open = (menu.open + MENUS.len() - 1) % MENUS.len();
                menu.item = 0;
            }
            KeyCode::Right | KeyCode::Tab => {
                menu.open = (menu.open + 1) % MENUS.len();
                menu.item = 0;
            }
            KeyCode::Up => menu.item = (menu.item + items - 1) % items,
            KeyCode::Down => menu.item = (menu.item + 1) % items,
            KeyCode::Enter => return self.act(MENUS[menu.open].1[menu.item].2),
            _ => {}
        }
        self.menu = Some(menu);
    }

    fn view_key(&mut self, key: KeyEvent) {
        // Views with their own keys (Search types into fields; Browse uses ← to go up)
        let outcome = match &mut self.view {
            View::Search(x) => {
                if x.in_results && key.code == KeyCode::Char('c') {
                    return self.copy_command();
                }
                Some(x.key(key))
            }
            View::Duplicates(x) => {
                if key.code == KeyCode::Char('f') {
                    let saved = x.filters.items.iter().map(|f| f.value.clone()).collect();
                    self.dialog = Some(Dialog::DupFilters { saved });
                    return;
                }
                if key.code == KeyCode::Char('c') {
                    return self.copy_command();
                }
                Some(x.key(key))
            }
            View::Browse(x) => {
                if key.code == KeyCode::Char('c') {
                    return self.copy_command();
                }
                Some(x.key(key))
            }
            _ => None,
        };
        match outcome {
            Some(Outcome::Back) => {
                self.view = View::Main;
                return;
            }
            Some(Outcome::Stay) => return,
            None => {}
        }
        let back = matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left);
        if back {
            self.view = View::Main;
            return;
        }
        if key.code == KeyCode::Char('c') {
            return self.copy_command();
        }
        match &mut self.view {
            View::Text { lines, scroll, .. } | View::Logs { lines, scroll, .. } => {
                let last = lines.len().saturating_sub(1);
                *scroll = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => (*scroll + 1).min(last),
                    KeyCode::PageUp => scroll.saturating_sub(20),
                    KeyCode::PageDown | KeyCode::Char(' ') => (*scroll + 20).min(last),
                    KeyCode::Home => 0,
                    KeyCode::End => last,
                    _ => *scroll,
                };
            }
            View::History { runs, selected, .. } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(runs.len().saturating_sub(1)),
                _ => {}
            },
            View::Status | View::Main | View::Search(_) | View::Duplicates(_) | View::Browse(_) => {}
        }
        // Keys that open another view
        match (&self.view, key.code) {
            (View::History { job, runs, selected }, KeyCode::Enter | KeyCode::Char('o')) => {
                let (job, run) = (job.clone(), runs.get(*selected).map(|r| r.id));
                let run = run.and_then(|id| job_history(&self.store, &job, None).ok()?.into_iter().find(|r| r.id == id));
                self.show_errors(run.as_ref(), &job);
            }
            (View::Logs { job, db, all, .. }, KeyCode::Char('a')) => {
                let (job, db, all) = (job.clone(), db.clone(), !*all);
                let lines = self.log_lines(job.as_deref(), &db, all);
                let scroll = lines.len().saturating_sub(1);
                self.view = View::Logs { job, db, all, lines, scroll };
            }
            _ => {}
        }
    }

    fn dialog_key(&mut self, dialog: Dialog, key: KeyEvent) {
        match dialog {
            Dialog::Help => {} // any key closes it
            Dialog::QuitWhileRunning => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    // Recorded as "stopped by user", like q in the indexing TUI
                    self.finish_running();
                    self.quit = true;
                }
                _ => {}
            },
            Dialog::RemoveJob { name, drop_index: drop } => match key.code {
                KeyCode::Char(' ') => self.dialog = Some(Dialog::RemoveJob { name, drop_index: !drop }),
                KeyCode::Char('y') | KeyCode::Enter => self.remove_job(&name, drop),
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => self.dialog = Some(Dialog::RemoveJob { name, drop_index: drop }),
            },
            Dialog::Reports { selected } => match key.code {
                KeyCode::Up => self.dialog = Some(Dialog::Reports { selected: selected.saturating_sub(1) }),
                KeyCode::Down => self.dialog = Some(Dialog::Reports { selected: (selected + 1).min(REPORTS.len() - 1) }),
                KeyCode::Enter => self.run_report(selected),
                KeyCode::Esc => {}
                _ => self.dialog = Some(Dialog::Reports { selected }),
            },
            Dialog::DupFilters { saved } => {
                let View::Duplicates(d) = &mut self.view else { return };
                match key.code {
                    KeyCode::Esc => {
                        for (f, v) in d.filters.items.iter_mut().zip(saved) {
                            f.value = v;
                        }
                    }
                    KeyCode::Enter => d.reload(),
                    KeyCode::Tab | KeyCode::Down => {
                        d.filters.next();
                        self.dialog = Some(Dialog::DupFilters { saved });
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        d.filters.prev();
                        self.dialog = Some(Dialog::DupFilters { saved });
                    }
                    _ => {
                        d.filters.edit(key);
                        self.dialog = Some(Dialog::DupFilters { saved });
                    }
                }
            }
            Dialog::Form(mut form) => match form.handle_key(key) {
                FormKey::Continue => self.dialog = Some(Dialog::Form(form)),
                FormKey::Cancel => {}
                FormKey::Submit => self.save_form(form),
            },
        }
    }

    fn run_report(&mut self, section: usize) {
        let Some(t) = self.target() else { return };
        let (label, flag) = REPORTS[section];
        let title = format!("{} — {}", label, t.label);
        let flag = if flag.is_empty() { String::new() } else { format!("{} ", flag) };
        let command = format!("diskunder report {} {}--db {}", t.path, flag, t.db);
        let width = crossterm::terminal::size().ok().map(|(w, _)| w.saturating_sub(2) as usize);
        self.spawn_text(title, command, move || {
            if !Path::new(&t.db).exists() {
                return Err(format!("nothing indexed yet in {} — run the job first", t.db));
            }
            let filters = crate::query::FilterArgs::default();
            let stale = crate::report::parse_age_arg("1y")?;
            let mut o = crate::report::plain_options(Some(&t.path), &filters);
            match section {
                1 => o.top_files = Some(None),
                2 => o.by_type = true,
                3 => o.by_directory = true,
                4 => o.stale = Some(&stale),
                5 => o.duplicates = true,
                _ => {}
            }
            crate::report::report_text(&t.db, &o, width)
        });
    }

    fn save_form(&mut self, mut form: Form) {
        let values = match form.values() {
            Ok(v) => v,
            Err(e) => {
                form.error = Some(e);
                self.dialog = Some(Dialog::Form(form));
                return;
            }
        };
        let settings = JobSettings {
            upsert: Some(values.upsert),
            path: Some(&values.path),
            db: Some(&values.db),
            threads: Some(values.threads),
            overrides: values.overrides.clone(),
        };
        let result = match &form.editing {
            None => create_job(&self.store, &values.name, &settings).map(|(job, warnings)| {
                let mut msg = format!("added job {} — ⏎ runs it", job.name);
                if let Some(w) = warnings.first() {
                    msg.push_str(&format!(" (note: {})", w));
                }
                msg
            }),
            Some(name) => crate::jobs::update_job(&self.store, name, &settings).map(|(changes, _)| {
                if changes.is_empty() {
                    format!("job {} unchanged", name)
                } else {
                    let fields: Vec<&str> = changes.iter().map(|c| c.0).collect();
                    format!("updated job {}: {}", name, fields.join(", "))
                }
            }),
        };
        match result {
            Ok(msg) => {
                self.message = Some(msg);
                self.refresh();
                if form.editing.is_none() {
                    if let Some(i) = self.jobs.iter().position(|s| s.job.name == values.name) {
                        self.selected = i;
                    }
                }
            }
            Err(e) => {
                form.error = Some(e);
                self.dialog = Some(Dialog::Form(form));
            }
        }
    }

    fn remove_job(&mut self, name: &str, drop: bool) {
        match delete_job(&self.store, name) {
            Ok(job) => {
                let mut msg = format!("removed job {}", name);
                if drop {
                    let mut runlog = RunLog::new(&job.db, self.log.as_deref(), self.no_log);
                    match drop_index(&job.db, &job.path, &mut runlog) {
                        Ok(summary) => msg.push_str(&format!(" — {}", summary)),
                        Err(e) => msg.push_str(&format!(" — its index wasn't removed: {}", e)),
                    }
                }
                self.message = Some(msg);
                self.refresh();
            }
            Err(e) => self.message = Some(e),
        }
    }

    fn copy_command(&mut self) {
        let cmd = self.command();
        self.message = Some(if copy_to_clipboard(&cmd) {
            format!("copied: {}", cmd)
        } else {
            "couldn't reach the clipboard — the command is shown above".to_string()
        });
    }
}

/// Clipboard via the platform's tool, else the terminal (OSC 52, which many terminals support)
fn copy_to_clipboard(text: &str) -> bool {
    use std::process::{Command, Stdio};
    for (cmd, args) in [("pbcopy", &[][..]), ("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
        if let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            let wrote = child.stdin.take().map(|mut s| s.write_all(text.as_bytes()).is_ok()).unwrap_or(false);
            if wrote && child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes())).is_ok() && out.flush().is_ok()
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Run the app until the user quits. Returns the exit code.
pub(crate) fn run_app(store: PathBuf, db: &str, log: Option<String>, no_log: bool) -> i32 {
    let mut app = App::new(store, db, log, no_log);
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let run = (|| -> Result<(), Box<dyn std::error::Error>> {
            while !app.quit {
                app.tick();
                terminal.draw(|f| draw::draw(f, &app))?;
                if event::poll(TICK)? {
                    if let Event::Key(key) = event::read()? {
                        app.handle_key(key);
                    }
                }
            }
            Ok(())
        })();
        // Always restore the terminal
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        run
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests;
