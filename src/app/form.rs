//! The create / edit job form

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::filters::{parse_path_pattern, Overrides, PathFilter, TypeFilter};
use crate::jobs::{check_name, Job};
use crate::query::parse_ext_list;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Text,
    Mode, // index / update, toggled with space or ←/→
}

pub(crate) struct Field {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) kind: Kind,
    pub(crate) hint: &'static str,
    pub(crate) locked: bool, // the name can't change when editing
}

pub(crate) struct Form {
    pub(crate) editing: Option<String>, // the job's name when editing; None when creating
    pub(crate) fields: Vec<Field>,
    pub(crate) focus: usize,
    pub(crate) error: Option<String>,
}

pub(crate) enum FormKey {
    Continue,
    Cancel,
    Submit,
}

/// What the form describes, checked
pub(crate) struct FormValues {
    pub(crate) name: String,
    pub(crate) upsert: bool,
    pub(crate) path: String,
    pub(crate) db: String,
    pub(crate) threads: usize,
    pub(crate) overrides: Overrides,
}

const NAME: usize = 0;
const MODE: usize = 1;
const PATH: usize = 2;
const DB: usize = 3;
const THREADS: usize = 4;
const INCLUDE_EXT: usize = 5;
const EXCLUDE_EXT: usize = 6;
const EXCLUDE_PATHS: usize = 7;
const INCLUDE_PATHS: usize = 8;

fn field(label: &'static str, value: String, kind: Kind, hint: &'static str) -> Field {
    Field { label, value, kind, hint, locked: false }
}

fn ext_list(exts: &[String]) -> String {
    exts.iter().map(|e| if e.is_empty() { "none" } else { e.as_str() }).collect::<Vec<_>>().join(",")
}

impl Form {
    pub(crate) fn new_job(db: &str) -> Self {
        Form {
            editing: None,
            fields: vec![
                field("Name", String::new(), Kind::Text, "letters, digits, - and _"),
                field("Mode", "update".into(), Kind::Mode, "space toggles: update keeps the index in line with disk"),
                field("Path", String::new(), Kind::Text, "the directory to index"),
                field("Database", db.to_string(), Kind::Text, "index database file — created on the first run"),
                field("Threads", "4".into(), Kind::Text, "parallel hashing; keep low for network shares"),
                field("Include types", String::new(), Kind::Text, "only these extensions, e.g. jpg,mov — blank for all"),
                field("Exclude types", String::new(), Kind::Text, "skip these extensions, e.g. tmp,log"),
                field("Exclude paths", String::new(), Kind::Text, "patterns, comma-separated, e.g. .snapshot, @eaDir"),
                field("Exceptions", String::new(), Kind::Text, "paths to index even if excluded, comma-separated"),
            ],
            focus: NAME,
            error: None,
        }
    }

    pub(crate) fn edit_job(job: &Job) -> Self {
        let mut f = Form::new_job(&job.db);
        f.editing = Some(job.name.clone());
        f.fields[NAME].value = job.name.clone();
        f.fields[NAME].locked = true;
        f.fields[MODE].value = job.mode().to_string();
        f.fields[PATH].value = job.path.clone();
        f.fields[THREADS].value = job.threads.to_string();
        f.fields[INCLUDE_EXT].value = job.filter.types.include.as_deref().map(ext_list).unwrap_or_default();
        f.fields[EXCLUDE_EXT].value = ext_list(&job.filter.types.exclude);
        f.fields[EXCLUDE_PATHS].value = job.filter.paths.exclude.join(", ");
        f.fields[INCLUDE_PATHS].value = job.filter.paths.include.join(", ");
        f.focus = MODE;
        f
    }

    pub(crate) fn title(&self) -> String {
        match &self.editing {
            Some(name) => format!(" Edit job {} ", name),
            None => " New job ".to_string(),
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let n = self.fields.len();
        loop {
            self.focus = if forward { (self.focus + 1) % n } else { (self.focus + n - 1) % n };
            if !self.fields[self.focus].locked {
                break;
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> FormKey {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormKey::Cancel,
            KeyCode::Enter => return FormKey::Submit,
            KeyCode::Char('s') if ctrl => return FormKey::Submit,
            KeyCode::Tab | KeyCode::Down => self.move_focus(true),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(false),
            _ => {
                let f = &mut self.fields[self.focus];
                match (f.kind, key.code) {
                    (Kind::Mode, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) => {
                        f.value = if f.value == "update" { "index".into() } else { "update".into() };
                    }
                    (Kind::Text, KeyCode::Char('u')) if ctrl => f.value.clear(),
                    (Kind::Text, KeyCode::Char(c)) if !ctrl => f.value.push(c),
                    (Kind::Text, KeyCode::Backspace) => {
                        f.value.pop();
                    }
                    _ => {}
                }
            }
        }
        self.error = None;
        FormKey::Continue
    }

    /// Check every field; on failure, focus the field at fault
    pub(crate) fn values(&mut self) -> Result<FormValues, String> {
        let v = |i: usize| self.fields[i].value.trim().to_string();
        let fail = |form: &mut Form, i: usize, msg: String| -> Result<FormValues, String> {
            form.focus = i;
            Err(msg)
        };
        let name = v(NAME);
        if let Err(e) = check_name(&name) {
            return fail(self, NAME, e);
        }
        if v(PATH).is_empty() {
            return fail(self, PATH, "a job needs a path".into());
        }
        if v(DB).is_empty() {
            return fail(self, DB, "a job needs a database file".into());
        }
        let threads = match v(THREADS).parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => return fail(self, THREADS, "threads must be a whole number above 0".into()),
        };
        let exts = |i: usize| -> Result<Option<Vec<String>>, String> {
            let s = v(i);
            if s.is_empty() { Ok(None) } else { parse_ext_list(&s).map(|l| Some(l.0)) }
        };
        let include = match exts(INCLUDE_EXT) {
            Ok(x) => x,
            Err(e) => return fail(self, INCLUDE_EXT, e),
        };
        let exclude = match exts(EXCLUDE_EXT) {
            Ok(x) => x.unwrap_or_default(),
            Err(e) => return fail(self, EXCLUDE_EXT, e),
        };
        let patterns = |i: usize| -> Result<Vec<String>, String> {
            v(i).split(',').map(str::trim).filter(|p| !p.is_empty()).map(parse_path_pattern).collect()
        };
        let exclude_paths = match patterns(EXCLUDE_PATHS) {
            Ok(p) => p,
            Err(e) => return fail(self, EXCLUDE_PATHS, e),
        };
        let include_paths = match patterns(INCLUDE_PATHS) {
            Ok(p) => p,
            Err(e) => return fail(self, INCLUDE_PATHS, e),
        };
        let paths = PathFilter::new(exclude_paths, include_paths)?;
        Ok(FormValues {
            name,
            upsert: v(MODE) == "update",
            path: v(PATH),
            db: v(DB),
            threads,
            overrides: Overrides { types: Some(TypeFilter { include, exclude }), paths: Some(paths) },
        })
    }

    /// The `diskunder jobs add …` command this form amounts to
    pub(crate) fn command(&self) -> String {
        let q = |s: &str| {
            if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c)) && !s.is_empty() {
                s.to_string()
            } else {
                format!("'{}'", s.replace('\'', r"'\''"))
            }
        };
        let v = |i: usize| self.fields[i].value.trim().to_string();
        let mut cmd = match &self.editing {
            Some(name) => format!("diskunder jobs edit {} --mode {} --path {}", name, v(MODE), q(&v(PATH))),
            None => format!("diskunder jobs add {} {} {}", q(&v(NAME)), v(MODE), q(&v(PATH))),
        };
        cmd.push_str(&format!(" --db {} --threads {}", q(&v(DB)), v(THREADS)));
        if !v(INCLUDE_EXT).is_empty() {
            cmd.push_str(&format!(" --include-ext {}", q(&v(INCLUDE_EXT))));
        }
        if !v(EXCLUDE_EXT).is_empty() {
            cmd.push_str(&format!(" --exclude-ext {}", q(&v(EXCLUDE_EXT))));
        }
        for (i, flag) in [(EXCLUDE_PATHS, "--exclude-path"), (INCLUDE_PATHS, "--include-path")] {
            for p in v(i).split(',').map(str::trim).filter(|p| !p.is_empty()) {
                cmd.push_str(&format!(" {} {}", flag, q(p)));
            }
        }
        cmd
    }
}
