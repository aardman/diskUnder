//! The interactive app: `diskunder` with no subcommand, in a terminal. A front end to the
//! jobs and runs the command line already has — every screen shows its equivalent command.

mod draw;
mod drives;
mod form;
mod results;
mod settings_view;
mod testjob;

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
use crate::host::{host_info, mounts};
use drives::{DrivesOutcome, DrivesView};
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
use settings_view::{SettingsOutcome, SettingsView};

use crate::maintenance::{backup, check_text, drop_table, info_text, vacuum};
use crate::settings::Settings;

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
    /// A scrollable report: Latest, Errors, a dry run, Host info
    Text { title: String, lines: Vec<String>, scroll: usize, command: String },
    History { job: String, runs: Vec<RunRow>, selected: usize },
    Logs { job: Option<String>, db: String, all: bool, lines: Vec<String>, scroll: usize },
    Search(Box<Search>),
    Duplicates(Box<Dups>),
    Browse(Box<Browse>),
    Settings(Box<SettingsView>),
    /// Drives & mounts, and the folders in them
    Drives(Box<DrivesView>),
}

/// An index the Database menu can drop: (label, table, the job that uses it)
pub(crate) struct DropItem {
    pub(crate) label: String,
    pub(crate) table: String,
    pub(crate) job: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum InputFor {
    Open,
    Backup,
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
    Vacuum,
    DropIndex { items: Vec<DropItem>, selected: usize, confirm: bool },
    Input { purpose: InputFor, value: String },
    /// The test job's steps; `made`/`ran`/`seen` tick them off, `clash` is a job of yours called test
    TestJob { selected: usize, made: bool, ran: bool, seen: bool, clash: bool },
}

/// The test job's steps: (label, what it does)
const TEST_RESULTS: &str = "Test job — results";

pub(crate) const TEST_STEPS: [(&str, &str); 4] = [
    ("Create", "sample files (photos, copies, a film project) and a job named test"),
    ("Run", "index the sample files — the results open when it finishes"),
    ("See results", "the run's summary; then try d duplicates, p reports, / search"),
    ("Remove", "the test job, its index and the sample files"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Action {
    NewJob,
    Run,
    DryRun,
    TestJob,
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
    DbInfo,
    DbCheck,
    DbVacuum,
    DbDrop,
    DbOpen,
    DbBackup,
    Drives,
    HostInfo,
    Settings(usize), // opens at this section
    Help,
    Quit,
}

/// The menu bar: (title, [(item, key hint, action)])
pub(crate) const MENUS: [(&str, &[(&str, &str, Action)]); 6] = [
    (
        "Jobs",
        &[
            ("Create…", "n", Action::NewJob),
            ("Run", "⏎", Action::Run),
            ("Dry run", "y", Action::DryRun),
            ("Edit…", "e", Action::Edit),
            ("Status", "s", Action::Status),
            ("Remove…", "x", Action::Remove),
            ("Test job…", "t", Action::TestJob),
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
    (
        "Database",
        &[
            ("Info", "", Action::DbInfo),
            ("Check", "", Action::DbCheck),
            ("Vacuum…", "", Action::DbVacuum),
            ("Drop path…", "", Action::DbDrop),
            ("Open…", "", Action::DbOpen),
            ("Backup…", "", Action::DbBackup),
        ],
    ),
    ("Host", &[("Drives & mounts", "", Action::Drives), ("Host info", "", Action::HostInfo)]),
    (
        "Settings",
        &[
            ("Start", "", Action::Settings(0)),
            ("Defaults", "", Action::Settings(1)),
            ("File types", "", Action::Settings(2)),
            ("Deletion", "", Action::Settings(3)),
            ("Logging", "", Action::Settings(4)),
        ],
    ),
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
    pub(crate) finished: Option<Finished>, // the strip once a run ends
    pending: Option<Pending>, // a dry run or report working in the background
    follow_test: bool,        // open the test job's results when its run ends
    test_seen: bool,          // the test job's results have been shown since it was made
    last_menu: Option<(usize, usize)>, // (menu, item) last chosen, for m to go back to
    pub(crate) message: Option<String>, // one-line feedback in the footer
    pub(crate) settings: Settings,
    pub(crate) quit: bool,
}

/// The last run, for the strip: (job, "completed in 3s at 12:01", the counts or the error)
pub(crate) struct Finished {
    pub(crate) job: String,
    pub(crate) headline: String,
    pub(crate) details: String,
}

/// A run's counts, short enough for one line
fn short_counts(c: &crate::run::RunCounts) -> String {
    let n = |x: usize| crate::fmt::fmt_count(x as i64);
    let mut s = format!(
        "{} files · {} written · {} removed · {} errors",
        n(c.found),
        n(c.written),
        n(c.removed),
        n(c.errors_unreadable + c.errors_unwritten + c.errors_other)
    );
    if c.dup_groups > 0 {
        s.push_str(&format!(" · {} duplicate groups ({} wasted)", n(c.dup_groups), crate::fmt::fmt_bytes(c.wasted)));
    }
    if c.excluded_files + c.excluded_folders > 0 {
        s.push_str(&format!(" · {} files, {} folders excluded", n(c.excluded_files), n(c.excluded_folders)));
    }
    s
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
            follow_test: false,
            test_seen: false,
            last_menu: None,
            message: None,
            settings: Settings::default(),
            quit: false,
        };
        app.settings = Settings::load(&app.store);
        app.refresh();
        if app.settings.yes("start.select_last_job") {
            let latest = app.jobs.iter().enumerate().filter_map(|(i, s)| s.last.as_ref().map(|r| (r.started.clone(), i))).max();
            if let Some((_, i)) = latest {
                app.selected = i;
            }
        }
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
        if self.finished.as_ref().is_some_and(|f| !self.jobs.iter().any(|s| s.job.name == f.job)) {
            self.finished = None; // its job was removed
        }
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
                    volume: r.volume.clone(),
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
        match &self.dialog {
            Some(Dialog::Form(f)) => return f.command(),
            Some(Dialog::Vacuum) => return format!("diskunder db vacuum --db {}", self.db),
            Some(Dialog::DropIndex { items, selected, .. }) => {
                if let Some(it) = items.get(*selected) {
                    return if it.label.starts_with("files_") {
                        format!("diskunder db drop --table {} --db {}", it.table, self.db)
                    } else {
                        format!("diskunder db drop {} --db {}", it.label, self.db)
                    };
                }
            }
            Some(Dialog::Input { purpose: InputFor::Open, value }) => return format!("diskunder --db {}", value),
            Some(Dialog::Input { purpose: InputFor::Backup, value }) => {
                return format!("diskunder db backup {} --db {}", value, self.db)
            }
            Some(Dialog::TestJob { selected, .. }) => {
                return match selected {
                    0 => testjob::add_command(&self.store),
                    1 => "diskunder run test".into(),
                    2 => "diskunder jobs show test".into(),
                    _ => "diskunder jobs remove test --drop-index".into(),
                }
            }
            _ => {}
        }
        match &self.view {
            View::Status => "diskunder jobs list".into(),
            View::Text { command, .. } => command.clone(),
            View::History { job, .. } => format!("diskunder jobs show {}", job),
            View::Logs { db, .. } => format!("less {}.log", db),
            View::Search(x) => x.command(),
            View::Duplicates(x) => x.command(),
            View::Browse(x) => x.command(),
            View::Settings(x) => x.command(&self.settings),
            View::Drives(x) => x.command(),
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
        let at = chrono::Local::now().format("%H:%M");
        let details = if outcome.outcome == "failed" { outcome.summary.clone() } else { short_counts(&outcome.counts) };
        self.finished = Some(Finished { job: run.name.clone(), headline: format!("{} in {} at {}", outcome.outcome, took, at), details });
        if let Some(reason) = run.runlog.failure() {
            self.message = Some(reason.to_string());
        }
        self.refresh();
        let waiting = self.menu.is_none() && (self.dialog.is_none() || matches!(self.dialog, Some(Dialog::TestJob { .. })));
        if run.name == testjob::NAME && std::mem::take(&mut self.follow_test) && matches!(self.view, View::Main) && waiting {
            self.dialog = None;
            self.show_test_results();
        }
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
                self.finished = Some(Finished { job: job.name.clone(), headline: "failed to start".into(), details: failed.summary });
                self.refresh();
            }
        }
    }

    fn dry_run(&mut self) {
        let Some(job) = self.selected_job() else {
            self.message = Some("select a job for a dry run".into());
            return;
        };
        let root = match fs::canonicalize(&job.path) {
            Ok(r) => r.to_string_lossy().into_owned(),
            Err(e) => {
                self.message = Some(format!("path not found: {} ({})", job.path, e));
                return;
            }
        };
        let title = format!("Dry run — {}", job.name);
        let command = format!("diskunder run {} --dry-run", job.name);
        self.spawn_text(title, command, move || {
            let (report, warnings) = dry_run_report(&job.db, &job.path, &root, job.upsert, &job.overrides())?;
            Ok((report, warnings.iter().map(|w| format!("warn: {}", w)).collect()))
        });
    }

    // ─── The test job ────────────────────────────────────────────────────────

    fn test_dialog(&self, selected: usize) -> Dialog {
        let job = self.jobs.iter().find(|s| s.job.name == testjob::NAME);
        let ours = job.is_some_and(|s| testjob::is_ours(&self.store, &s.job));
        let ran = ours && job.is_some_and(|s| s.last.is_some());
        Dialog::TestJob { selected, made: ours, ran, seen: ran && self.test_seen, clash: job.is_some() && !ours }
    }

    fn select_test_job(&mut self) -> bool {
        match self.jobs.iter().position(|s| s.job.name == testjob::NAME) {
            Some(i) => {
                self.selected = i;
                true
            }
            None => {
                self.message = Some("create the test job first".into());
                false
            }
        }
    }

    fn test_step(&mut self, step: usize, made: bool, clash: bool) {
        if clash && step != 2 {
            self.message = Some("you have a job called test — rename or remove it to use the test job".into());
            return;
        }
        match step {
            0 if made => {
                self.message = Some("the test job is ready — run it next".into());
                self.dialog = Some(self.test_dialog(1));
            }
            0 => match testjob::create(&self.store) {
                Ok(msg) => {
                    self.message = Some(msg);
                    self.test_seen = false;
                    self.refresh();
                    self.select_test_job();
                    self.dialog = Some(self.test_dialog(1));
                }
                Err(e) => self.message = Some(e),
            },
            1 => {
                if made && self.select_test_job() {
                    self.run_job();
                    self.follow_test = self.running.as_ref().is_some_and(|r| r.name == testjob::NAME);
                    if self.follow_test {
                        // The steps stay up while it runs; the results replace them at the end
                        self.dialog = Some(self.test_dialog(1));
                    }
                } else if !made {
                    self.message = Some("create the test job first".into());
                }
            }
            2 => {
                if self.select_test_job() {
                    self.show_test_results();
                }
            }
            _ => {
                if self.running.as_ref().is_some_and(|r| r.name == testjob::NAME) {
                    self.message = Some("the test job is running — wait for it to finish".into());
                    return;
                }
                let job = self.jobs.iter().find(|s| s.job.name == testjob::NAME).map(|s| s.job.clone());
                self.message = Some(testjob::remove(&self.store, job.as_ref()).unwrap_or_else(|e| e));
                self.test_seen = false;
                self.refresh();
            }
        }
    }

    /// Latest for the test job, with what to try next
    fn show_test_results(&mut self) {
        if !self.select_test_job() {
            return;
        }
        self.show_latest();
        self.test_seen = true;
        if let View::Text { title, lines, .. } = &mut self.view {
            *title = TEST_RESULTS.into();
            lines.extend(
                [
                    "",
                    "The sample has copies of photos in backup/ and an old copy of a project file, so try",
                    "(Esc twice to the main window first):  d duplicates · p reports · / search · b browse · y dry run",
                    "The job skips *.tmp files and .snapshot folders — Browse shows they're not indexed.",
                    "Esc goes back to the test job's steps — Remove deletes the job and its sample files.",
                ]
                .map(String::from),
            );
        }
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
            Action::NewJob => self.dialog = Some(Dialog::Form(Form::new_job(&self.db, &self.settings))),
            Action::Run => self.run_job(),
            Action::DryRun => self.dry_run(),
            Action::TestJob => {
                let next = match self.test_dialog(0) {
                    Dialog::TestJob { made: false, .. } => 0,
                    Dialog::TestJob { ran: false, .. } => 1,
                    Dialog::TestJob { seen: false, .. } => 2,
                    _ => 3,
                };
                self.dialog = Some(self.test_dialog(next));
            }
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
                    self.view = View::Search(Box::new(Search::new(t, self.settings.clone())));
                }
            }
            Action::Duplicates => {
                if let Some(t) = self.target() {
                    self.view = View::Duplicates(Box::new(Dups::new(t, self.settings.clone())));
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
            Action::DbInfo => {
                let db = self.db.clone();
                self.spawn_text(format!("Database — {}", db), format!("diskunder db info --db {}", db), move || {
                    info_text(&db).map(|t| (t, Vec::new()))
                });
            }
            Action::DbCheck => {
                let db = self.db.clone();
                self.spawn_text(format!("Check — {}", db), format!("diskunder db check --db {}", db), move || {
                    check_text(&db).map(|(t, _)| (t, Vec::new()))
                });
            }
            Action::DbVacuum => self.dialog = Some(Dialog::Vacuum),
            Action::DbDrop => self.choose_drop(),
            Action::DbOpen => self.dialog = Some(Dialog::Input { purpose: InputFor::Open, value: self.db.clone() }),
            Action::DbBackup => {
                let stamp = chrono::Local::now().format("%Y%m%d-%H%M").to_string();
                self.dialog = Some(Dialog::Input { purpose: InputFor::Backup, value: format!("{}.backup-{}", self.db, stamp) });
            }
            Action::Drives => {
                let jobs = self.jobs.iter().map(|s| (s.job.path.clone(), s.job.name.clone())).collect();
                self.view = View::Drives(Box::new(DrivesView::new(mounts(), jobs)));
            }
            Action::HostInfo => {
                let log = self.settings.get("log.folder").to_string();
                let lines = host_info(&self.store, &self.db, &log).into_iter().map(|(k, v)| format!("{:<11} {}", k, v)).collect();
                self.view = View::Text { title: "Host info".into(), lines, scroll: 0, command: "diskunder host".into() };
            }
            Action::Settings(section) => {
                self.view = View::Settings(Box::new(SettingsView::new(&self.store, &self.settings, section)));
            }
            Action::Help => self.dialog = Some(Dialog::Help),
            Action::Quit => self.request_quit(),
        }
    }

    /// m from a view or dialog: back to the menu it was opened from (the test job's steps,
    /// for its results)
    fn back_to_menu(&mut self) {
        let from_test = matches!(&self.view, View::Text { title, .. } if title == TEST_RESULTS);
        self.view = View::Main;
        self.dialog = None;
        if from_test {
            self.dialog = Some(self.test_dialog(3));
        } else {
            let (open, item) = self.last_menu.unwrap_or((0, 0));
            self.menu = Some(Menu { open, item });
        }
    }

    /// Whether the screen is taking typed text, so m is a letter, not "back to the menu"
    fn typing(&self) -> bool {
        match (&self.dialog, &self.view) {
            (Some(Dialog::Form(_) | Dialog::Input { .. } | Dialog::DupFilters { .. }), _) => true,
            (Some(_), _) => false,
            (None, View::Search(x)) => !x.in_results,
            (None, View::Settings(x)) => x.editing.is_some(),
            _ => false,
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
        let elsewhere = self.dialog.is_some() || !matches!(self.view, View::Main);
        if key.code == KeyCode::Char('m') && elsewhere && self.menu.is_none() && !self.typing() {
            return self.back_to_menu();
        }
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
            KeyCode::Char('y') => self.act(Action::DryRun),
            KeyCode::Char('t') => self.act(Action::TestJob),
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
            KeyCode::Enter => {
                self.last_menu = Some((menu.open, menu.item));
                return self.act(MENUS[menu.open].1[menu.item].2);
            }
            _ => {}
        }
        self.menu = Some(menu);
    }

    fn view_key(&mut self, key: KeyEvent) {
        if let View::Settings(v) = &mut self.view {
            if v.editing.is_none() && key.code == KeyCode::Char('c') {
                return self.copy_command();
            }
            match v.key(key, &self.settings) {
                SettingsOutcome::Stay => {}
                SettingsOutcome::Back => self.view = View::Main,
                SettingsOutcome::Changed(msg) => {
                    self.settings = Settings::load(&self.store);
                    self.message = Some(msg);
                }
                SettingsOutcome::Message(msg) => self.message = Some(msg),
            }
            return;
        }
        if let View::Drives(x) = &mut self.view {
            if key.code == KeyCode::Char('c') {
                return self.copy_command();
            }
            match x.key(key) {
                DrivesOutcome::Stay => {}
                DrivesOutcome::Back => self.view = View::Main,
                DrivesOutcome::Message(m) => self.message = Some(m),
                DrivesOutcome::Use(path) => {
                    self.view = View::Main;
                    self.dialog = Some(Dialog::Form(Form::new_job_at(&self.db, &self.settings, &path)));
                }
            }
            return;
        }
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
            if matches!(&self.view, View::Text { title, .. } if title == TEST_RESULTS) {
                return self.back_to_menu();
            }
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
            View::Status
            | View::Main
            | View::Search(_)
            | View::Duplicates(_)
            | View::Browse(_)
            | View::Settings(_)
            | View::Drives(_) => {}
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
            Dialog::Vacuum => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                    let (db, log, no_log) = (self.db.clone(), self.log.clone(), self.no_log);
                    self.spawn_text(format!("Vacuum — {}", db), format!("diskunder db vacuum --db {}", db), move || {
                        let mut runlog =
                            RunLog::new(&db, log.as_deref(), no_log).for_command(format!("db vacuum --db {} [app]", db));
                        vacuum(&db, &mut runlog).map(|t| (t, Vec::new()))
                    });
                }
            }
            Dialog::DropIndex { items, selected, confirm } => match (confirm, key.code) {
                (false, KeyCode::Up) => self.dialog = Some(Dialog::DropIndex { items, selected: selected.saturating_sub(1), confirm }),
                (false, KeyCode::Down) => {
                    let selected = (selected + 1).min(items.len().saturating_sub(1));
                    self.dialog = Some(Dialog::DropIndex { items, selected, confirm });
                }
                (false, KeyCode::Enter) if !items.is_empty() => self.dialog = Some(Dialog::DropIndex { items, selected, confirm: true }),
                (true, KeyCode::Char('y')) => {
                    let item = &items[selected];
                    let mut runlog = RunLog::new(&self.db, self.log.as_deref(), self.no_log)
                        .for_command(format!("db drop --table {} --db {} [app]", item.table, self.db));
                    self.message = Some(match drop_table(&self.db, &item.table, &mut runlog) {
                        Ok(summary) => summary,
                        Err(e) => e,
                    });
                    self.refresh();
                }
                (_, KeyCode::Esc) | (true, _) => {}
                _ => self.dialog = Some(Dialog::DropIndex { items, selected, confirm }),
            },
            Dialog::Input { purpose, mut value } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_input(purpose, value.trim().to_string()),
                KeyCode::Backspace => {
                    value.pop();
                    self.dialog = Some(Dialog::Input { purpose, value });
                }
                KeyCode::Char('u') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                    self.dialog = Some(Dialog::Input { purpose, value: String::new() });
                }
                KeyCode::Char(c) => {
                    value.push(c);
                    self.dialog = Some(Dialog::Input { purpose, value });
                }
                _ => self.dialog = Some(Dialog::Input { purpose, value }),
            },
            Dialog::TestJob { selected, made, ran, seen, clash } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.dialog = Some(Dialog::TestJob { selected: selected.saturating_sub(1), made, ran, seen, clash })
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let selected = (selected + 1).min(TEST_STEPS.len() - 1);
                    self.dialog = Some(Dialog::TestJob { selected, made, ran, seen, clash })
                }
                KeyCode::Char(c @ '1'..='4') => self.test_step(c as usize - '1' as usize, made, clash),
                KeyCode::Enter => self.test_step(selected, made, clash),
                KeyCode::Char('c') => {
                    self.dialog = Some(Dialog::TestJob { selected, made, ran, seen, clash });
                    self.copy_command();
                }
                KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.dialog = Some(Dialog::TestJob { selected, made, ran, seen, clash }),
            },
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

    /// Every index in the open database, with the job that uses it (if any)
    fn choose_drop(&mut self) {
        let Ok(conn) = Connection::open(&self.db) else {
            self.message = Some(format!("can't open {}", self.db));
            return;
        };
        let roots = load_roots(&conn).unwrap_or_default();
        let items: Vec<DropItem> = file_tables(&conn)
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                let root = roots.get(&t).map(|r| r.root.clone());
                let job = root.as_ref().and_then(|r| {
                    self.jobs.iter().find(|s| s.job.db == self.db && &s.job.path == r).map(|s| s.job.name.clone())
                });
                DropItem { label: root.unwrap_or_else(|| t.clone()), table: t, job }
            })
            .collect();
        if items.is_empty() {
            self.message = Some(format!("nothing is indexed in {}", self.db));
            return;
        }
        self.dialog = Some(Dialog::DropIndex { items, selected: 0, confirm: false });
    }

    fn submit_input(&mut self, purpose: InputFor, value: String) {
        if value.is_empty() {
            return;
        }
        match purpose {
            InputFor::Open => {
                self.db = absolute(&value);
                self.selected = 0;
                self.refresh();
                self.message = Some(format!(
                    "opened {}{}",
                    self.db,
                    if Path::new(&self.db).exists() { "" } else { " — nothing indexed there yet" }
                ));
            }
            InputFor::Backup => {
                let db = self.db.clone();
                let command = format!("diskunder db backup {} --db {}", value, db);
                self.spawn_text(format!("Backup — {}", db), command, move || backup(&db, &value).map(|t| (t, Vec::new())));
            }
        }
    }

    fn save_form(&mut self, mut form: Form) {
        let values = match form.values(&self.settings) {
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
                    let mut runlog = RunLog::new(&job.db, self.log.as_deref(), self.no_log)
                        .for_command(format!("jobs remove {} --drop-index [app]", name));
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
