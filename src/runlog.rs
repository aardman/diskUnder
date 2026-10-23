//! Run log: a line per event for every command that changes a database — what was run, how
//! it ended, a summary, and any errors with running it (never per-file detail)

use chrono::{Local, SecondsFormat};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::cli::display_version;
use crate::scan::WorkerMsg;

/// Where run logs go and when they rotate — from settings, set once at startup
struct LogConfig {
    folder: Option<PathBuf>, // None: next to each database
    rotate_at: u64,          // bytes; the log is renamed to <log>.1 beyond this
}

static CONFIG: OnceLock<LogConfig> = OnceLock::new();

pub(crate) fn configure(folder: Option<PathBuf>, rotate_mb: u64) {
    let _ = CONFIG.set(LogConfig { folder, rotate_at: rotate_mb.max(1) * 1024 * 1024 });
}

fn rotate_at() -> u64 {
    CONFIG.get().map_or(10 * 1024 * 1024, |c| c.rotate_at)
}
const MAX_ERRORS: usize = 50; // per run; any beyond this are counted, not written

pub(crate) struct RunLog {
    path: Option<PathBuf>, // None when disabled, or after a write failed
    id: String,
    started: Instant,
    finished: Option<Instant>, // when the work ended, which can be before `end` is called
    errors_logged: usize,
    errors_dropped: usize,
    errors: Vec<String>, // the errors logged (up to MAX_ERRORS), kept for job history
    failure: Option<String>,
    command: Option<String>, // what `start` records, instead of this process's command line
}

impl RunLog {
    /// Logs to `log`, or `<db_path>.log` by default; `disabled` is `--no-log`
    pub(crate) fn new(db_path: &str, log: Option<&str>, disabled: bool) -> Self {
        // --log, else the log folder from settings, else beside the database
        let path = (!disabled).then(|| match (log, CONFIG.get().and_then(|c| c.folder.as_ref())) {
            (Some(l), _) => PathBuf::from(l),
            (None, Some(folder)) => {
                let name = Path::new(db_path).file_name().map_or("index.db".into(), |n| n.to_string_lossy().into_owned());
                let _ = fs::create_dir_all(folder);
                folder.join(format!("{}.log", name))
            }
            (None, None) => PathBuf::from(format!("{}.log", db_path)),
        });
        // Short id tying together the lines of one run when two runs share a log
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let id = format!("{:06x}", (nanos ^ std::process::id().wrapping_mul(2_654_435_761)) & 0xff_ffff);
        RunLog {
            path,
            id,
            started: Instant::now(),
            finished: None,
            errors_logged: 0,
            errors_dropped: 0,
            errors: Vec::new(),
            failure: None,
            command: None,
        }
    }

    /// Start, recording this process's command line (or the command set with `for_command`)
    pub(crate) fn start(&mut self) {
        let command = self.command.clone().unwrap_or_else(|| {
            std::env::args().skip(1).map(|a| shell_quote(&a)).collect::<Vec<_>>().join(" ")
        });
        self.start_as(&command);
    }

    /// Record `command` as what was run — for actions the app takes, which has no command line
    pub(crate) fn for_command(mut self, command: String) -> Self {
        self.command = Some(command);
        self
    }

    /// Start, recording `command` — for runs started from the app, which has no command line
    pub(crate) fn start_as(&mut self, command: &str) {
        self.rotate_if_large();
        self.started = Instant::now();
        let line = format!("{}  (user={} host={} version={})", command, user(), host(), display_version());
        self.write("start", &line);
    }

    pub(crate) fn info(&mut self, text: &str) {
        self.write("info", text);
    }

    pub(crate) fn error(&mut self, text: &str) {
        if self.errors_logged < MAX_ERRORS {
            self.errors_logged += 1;
            self.errors.push(text.replace('\n', " "));
            self.write("error", text);
        } else {
            self.errors_dropped += 1;
        }
    }

    /// Record what a worker message reports, if it matters at run level. Per-file read
    /// errors (`FileError`) are deliberately left out — they're counted in the summary.
    pub(crate) fn observe(&mut self, msg: &WorkerMsg) {
        match msg {
            WorkerMsg::Info(line) => self.info(line),
            WorkerMsg::Error { context, error } | WorkerMsg::Fatal { context, error } => {
                self.error(&format!("{} — {}", context, error))
            }
            WorkerMsg::BatchFailed { files, error } => {
                self.error(&format!("batch of {} files not written — {}", files, error))
            }
            // The TUI stays open after the work is done; don't count time spent looking at it
            WorkerMsg::Done => self.finished = Some(Instant::now()),
            _ => {}
        }
    }

    /// `outcome` is e.g. "completed", "failed" or "stopped by user"
    pub(crate) fn end(&mut self, outcome: &str, summary: &str, exit_code: i32) {
        if self.errors_dropped > 0 {
            self.write("error", &format!("… and {} more errors (not logged)", self.errors_dropped));
        }
        let code = if exit_code != 0 { format!(" (exit {})", exit_code) } else { String::new() };
        let took = self.finished.unwrap_or_else(Instant::now) - self.started;
        let line = format!("{} in {}{} — {}", outcome, fmt_duration(took), code, summary);
        self.write("end", &line);
    }

    /// Ties log lines and job history together
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The errors this run logged (at most 50), and how many more there were
    pub(crate) fn errors(&self) -> (&[String], usize) {
        (&self.errors, self.errors_dropped)
    }

    /// How long the work took, in whole seconds
    pub(crate) fn duration_secs(&self) -> u64 {
        (self.finished.unwrap_or_else(Instant::now) - self.started).as_secs()
    }

    /// Why the log couldn't be written, if it couldn't. Logging never stops a run; the
    /// caller reports this once, after the run (and after the TUI has closed).
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn write(&mut self, kind: &str, text: &str) {
        let Some(path) = &self.path else { return };
        let line = format!(
            "{} run={} {:<6} {}\n",
            Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
            self.id,
            kind,
            text.replace('\n', " ")
        );
        // A single append per line, so lines from concurrent runs never interleave mid-line
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = result {
            self.failure = Some(format!("could not write run log {}: {}", path.display(), e));
            self.path = None;
        }
    }

    fn rotate_if_large(&self) {
        if let Some(path) = &self.path {
            if fs::metadata(path).is_ok_and(|m| m.len() >= rotate_at()) {
                let _ = fs::rename(path, format!("{}.1", path.display()));
            }
        }
    }
}

/// Quote an argument only when the shell would need it, so logged commands can be re-run
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// 14m33s, 2h05m, 9s
fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{}s", s),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// cron usually sets LOGNAME but not USER; USERNAME covers Windows
pub(crate) fn user() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(unix)]
pub(crate) fn host() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes, and gethostname writes at most that many
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    if !ok {
        return "unknown".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
pub(crate) fn host() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
}
