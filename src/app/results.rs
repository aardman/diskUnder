//! Results views that query an index: Search, Duplicates and Browse

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rusqlite::{params_from_iter, types::Value, Connection};
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

use crate::output::FileRow;
use crate::query::{build_parts, build_query, parse_age, parse_ext_list, parse_size, resolve_scope, ExtList, FilterArgs, SortKey};
use crate::report::{duplicate_groups_for, group_members_for, plain_options, GroupInfo, Member};
use crate::settings::Settings;

const SEARCH_ROWS: usize = 200;
const SEARCH_PAUSE: Duration = Duration::from_millis(300); // re-run once typing pauses

/// What a results view looks at: an index database and a path in it
#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) label: String, // job name, or the path
    pub(crate) db: String,
    pub(crate) path: String,
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn open(target: &Target) -> Result<Connection, String> {
    if !Path::new(&target.db).exists() {
        return Err(format!("nothing indexed yet in {} — run the job first", target.db));
    }
    Connection::open(&target.db).map_err(|e| e.to_string())
}

// ─── Editable fields ─────────────────────────────────────────────────────────

pub(crate) struct FieldItem {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) hint: &'static str,
    pub(crate) choices: &'static [&'static str], // non-empty: space cycles through these
}

/// A column of single-line fields: Tab / ↑↓ move, typing edits, space cycles choices
pub(crate) struct Fields {
    pub(crate) items: Vec<FieldItem>,
    pub(crate) focus: usize,
}

impl Fields {
    fn text(label: &'static str, hint: &'static str) -> FieldItem {
        FieldItem { label, value: String::new(), hint, choices: &[] }
    }

    fn choice(label: &'static str, choices: &'static [&'static str], hint: &'static str) -> FieldItem {
        FieldItem { label, value: choices[0].to_string(), hint, choices }
    }

    pub(crate) fn get(&self, label: &str) -> &str {
        self.items.iter().find(|f| f.label == label).map_or("", |f| f.value.trim())
    }

    /// Edit the focused field; returns whether a value changed
    pub(crate) fn edit(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let f = &mut self.items[self.focus];
        match key.code {
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right if !f.choices.is_empty() => {
                let i = f.choices.iter().position(|c| *c == f.value).unwrap_or(0);
                let n = f.choices.len();
                let next = if key.code == KeyCode::Left { (i + n - 1) % n } else { (i + 1) % n };
                f.value = f.choices[next].to_string();
                true
            }
            KeyCode::Char('u') if ctrl => {
                f.value.clear();
                true
            }
            KeyCode::Char(c) if !ctrl && f.choices.is_empty() => {
                f.value.push(c);
                true
            }
            KeyCode::Backspace if f.choices.is_empty() => f.value.pop().is_some(),
            _ => false,
        }
    }

    pub(crate) fn next(&mut self) {
        self.focus = (self.focus + 1) % self.items.len();
    }

    pub(crate) fn prev(&mut self) {
        self.focus = (self.focus + self.items.len() - 1) % self.items.len();
    }
}

pub(crate) enum Outcome {
    Stay,
    Back,
}

// ─── Search ──────────────────────────────────────────────────────────────────

pub(crate) struct Search {
    pub(crate) target: Target,
    settings: Settings, // for @groups in Types
    pub(crate) fields: Fields,
    pub(crate) results: Vec<FileRow>,
    pub(crate) total: usize,
    pub(crate) error: Option<String>,
    pub(crate) in_results: bool,
    pub(crate) selected: usize,
    changed: Option<Instant>,
}

const SORTS: &[&str] = &["path", "size", "modified", "name"];

impl Search {
    pub(crate) fn new(target: Target, settings: Settings) -> Self {
        let fields = Fields {
            items: vec![
                Fields::text("Name", "e.g. *.mov — * and ? wildcards"),
                Fields::text("Types", "extensions, e.g. jpg,mov or @video (none = no extension)"),
                Fields::text("Larger than", "e.g. 500MB, 1.5GB"),
                Fields::text("Smaller than", "e.g. 10K"),
                Fields::text("Older than", "modified before: 90d, 6w, 3m, 1y or 2024-01-01"),
                Fields::text("Newer than", "modified within: 7d, 2024-06-01"),
                Fields::text("Under", "only this folder"),
                Fields::choice("Duplicates", &["no", "only"], "space: only files with another copy"),
                Fields::choice("Sort", SORTS, "space: path, size, modified, name"),
            ],
            focus: 0,
        };
        let mut s = Search { target, settings, fields, results: Vec::new(), total: 0, error: None, in_results: false, selected: 0, changed: None };
        s.refresh();
        s
    }

    fn filters(&self) -> Result<(FilterArgs, SortKey), String> {
        let f = &self.fields;
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let mut filters = FilterArgs { name: opt(f.get("Name")), under: opt(f.get("Under")), ..FilterArgs::default() };
        if !f.get("Types").is_empty() {
            filters.ext = Some(parse_ext_list(&self.settings.expand_types(f.get("Types"))?)?);
        }
        if !f.get("Larger than").is_empty() {
            filters.larger_than = Some(parse_size(f.get("Larger than"))?);
        }
        if !f.get("Smaller than").is_empty() {
            filters.smaller_than = Some(parse_size(f.get("Smaller than"))?);
        }
        if !f.get("Older than").is_empty() {
            filters.older_than = Some(parse_age(f.get("Older than"))?);
        }
        if !f.get("Newer than").is_empty() {
            filters.newer_than = Some(parse_age(f.get("Newer than"))?);
        }
        filters.duplicate = f.get("Duplicates") == "only";
        let sort = match f.get("Sort") {
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "name" => SortKey::Name,
            _ => SortKey::Path,
        };
        Ok((filters, sort))
    }

    pub(crate) fn refresh(&mut self) {
        self.changed = None;
        let result = (|| -> Result<(Vec<FileRow>, usize), String> {
            let (filters, sort) = self.filters()?;
            let conn = open(&self.target)?;
            let scope = resolve_scope(&conn, Some(&self.target.path))?;
            let q = build_query(&conn, &scope, &filters, sort, Some(SEARCH_ROWS + 1)).map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare(&q.sql).map_err(|e| e.to_string())?;
            let mut rows: Vec<FileRow> = stmt
                .query_map(params_from_iter(q.params.iter()), FileRow::from_row)
                .map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<_>>()
                .map_err(|e| e.to_string())?;
            let total = if rows.len() > SEARCH_ROWS {
                rows.truncate(SEARCH_ROWS);
                conn.query_row(&q.count_sql, params_from_iter(q.params.iter()), |r| r.get::<_, i64>(0))
                    .map_err(|e| e.to_string())? as usize
            } else {
                rows.len()
            };
            Ok((rows, total))
        })();
        match result {
            Ok((rows, total)) => {
                self.results = rows;
                self.total = total;
                self.error = None;
                self.selected = self.selected.min(self.results.len().saturating_sub(1));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Re-run once typing has paused
    pub(crate) fn tick(&mut self) {
        if self.changed.is_some_and(|t| t.elapsed() >= SEARCH_PAUSE) {
            self.refresh();
        }
    }

    pub(crate) fn command(&self) -> String {
        let f = &self.fields;
        let mut cmd = format!("diskunder search {}", quote(&self.target.path));
        for (label, flag) in [
            ("Name", "--name"),
            ("Types", "--ext"),
            ("Larger than", "--larger-than"),
            ("Smaller than", "--smaller-than"),
            ("Older than", "--older-than"),
            ("Newer than", "--newer-than"),
            ("Under", "--under"),
        ] {
            if !f.get(label).is_empty() {
                cmd.push_str(&format!(" {} {}", flag, quote(f.get(label))));
            }
        }
        if f.get("Duplicates") == "only" {
            cmd.push_str(" --duplicate");
        }
        if f.get("Sort") != "path" {
            cmd.push_str(&format!(" --sort {}", f.get("Sort")));
        }
        cmd.push_str(&format!(" --db {}", quote(&self.target.db)));
        cmd
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.code == KeyCode::Esc {
            return Outcome::Back;
        }
        if self.in_results {
            match key.code {
                KeyCode::Up if self.selected == 0 => self.in_results = false,
                KeyCode::Up => self.selected -= 1,
                KeyCode::Down => self.selected = (self.selected + 1).min(self.results.len().saturating_sub(1)),
                KeyCode::PageDown => self.selected = (self.selected + 20).min(self.results.len().saturating_sub(1)),
                KeyCode::PageUp => self.selected = self.selected.saturating_sub(20),
                KeyCode::Tab | KeyCode::BackTab => self.in_results = false,
                _ => {}
            }
            return Outcome::Stay;
        }
        match key.code {
            KeyCode::Tab => self.fields.next(),
            KeyCode::BackTab | KeyCode::Up => self.fields.prev(),
            KeyCode::Down if self.fields.focus + 1 == self.fields.items.len() => {
                self.in_results = !self.results.is_empty();
            }
            KeyCode::Down => self.fields.next(),
            KeyCode::Enter => {
                self.refresh();
                self.in_results = !self.results.is_empty();
            }
            _ => {
                if self.fields.edit(key) {
                    self.changed = Some(Instant::now());
                }
            }
        }
        Outcome::Stay
    }
}

// ─── Duplicates ──────────────────────────────────────────────────────────────

pub(crate) struct Dups {
    pub(crate) target: Target,
    settings: Settings, // for @groups in Types
    pub(crate) filters: Fields,
    pub(crate) groups: Vec<GroupInfo>,
    pub(crate) selected: usize,
    pub(crate) members: Vec<Member>,
    pub(crate) error: Option<String>,
    pub(crate) all: bool, // every path in the database, not just the target's
}

impl Dups {
    pub(crate) fn new(target: Target, settings: Settings) -> Self {
        let filters = Fields {
            items: vec![
                Fields::text("Min size", "each copy at least, e.g. 100MB"),
                Fields::text("Min copies", "distinct copies (hard links count once), e.g. 3"),
                Fields::text("Min wasted", "size × (copies − 1) at least, e.g. 1GB"),
                Fields::text("Types", "groups with a copy of these types, e.g. iso,dmg"),
                Fields::text("Under", "groups with a copy under this folder"),
            ],
            focus: 0,
        };
        let mut d = Dups { target, settings, filters, groups: Vec::new(), selected: 0, members: Vec::new(), error: None, all: false };
        d.reload();
        d
    }

    /// The path the view covers; None for every path in the database
    fn scope(&self) -> Option<&str> {
        (!self.all).then_some(self.target.path.as_str())
    }

    /// What the view covers, for its title
    pub(crate) fn label(&self) -> String {
        if self.all {
            format!("all paths in {}", Path::new(&self.target.db).file_name().map_or(self.target.db.clone(), |n| n.to_string_lossy().into_owned()))
        } else {
            self.target.label.clone()
        }
    }

    /// (shared filters, min size, min copies, min wasted) from the filter fields
    fn parsed(&self) -> Result<(FilterArgs, Option<u64>, usize, Option<u64>), String> {
        let f = &self.filters;
        let size = |label: &str| -> Result<Option<u64>, String> {
            if f.get(label).is_empty() { Ok(None) } else { parse_size(f.get(label)).map(Some) }
        };
        let copies = if f.get("Min copies").is_empty() {
            2
        } else {
            f.get("Min copies").parse::<usize>().map_err(|_| "min copies must be a whole number".to_string())?
        };
        let ext: Option<ExtList> =
            if f.get("Types").is_empty() { None } else { Some(parse_ext_list(&self.settings.expand_types(f.get("Types"))?)?) };
        let under = (!f.get("Under").is_empty()).then(|| f.get("Under").to_string());
        Ok((FilterArgs { ext, under, ..FilterArgs::default() }, size("Min size")?, copies, size("Min wasted")?))
    }

    pub(crate) fn reload(&mut self) {
        let result = (|| -> Result<Vec<GroupInfo>, String> {
            let (filters, min_size, min_copies, min_wasted) = self.parsed()?;
            open(&self.target)?;
            let mut o = plain_options(self.scope(), &filters);
            o.min_size = min_size;
            o.min_copies = min_copies;
            o.min_wasted = min_wasted;
            duplicate_groups_for(&self.target.db, &o)
        })();
        match result {
            Ok(groups) => {
                self.groups = groups;
                self.error = None;
                self.selected = 0;
                self.load_members();
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn load_members(&mut self) {
        self.members = match self.groups.get(self.selected) {
            Some(g) => {
                let filters = FilterArgs::default();
                let o = plain_options(self.scope(), &filters);
                group_members_for(&self.target.db, &o, &g.checksum).unwrap_or_default()
            }
            None => Vec::new(),
        };
    }

    pub(crate) fn wasted(&self) -> i64 {
        self.groups.iter().map(|g| g.wasted).sum()
    }

    pub(crate) fn command(&self) -> String {
        let f = &self.filters;
        let mut cmd = match self.scope() {
            Some(p) => format!("diskunder report {} --duplicates", quote(p)),
            None => "diskunder report --duplicates".to_string(),
        };
        for (label, flag) in [
            ("Min size", "--min-size"),
            ("Min copies", "--min-copies"),
            ("Min wasted", "--min-wasted"),
            ("Types", "--ext"),
            ("Under", "--under"),
        ] {
            if !f.get(label).is_empty() {
                cmd.push_str(&format!(" {} {}", flag, quote(f.get(label))));
            }
        }
        cmd.push_str(&format!(" --db {}", quote(&self.target.db)));
        cmd
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Outcome {
        let before = self.selected;
        match key.code {
            KeyCode::Esc => return Outcome::Back,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(self.groups.len().saturating_sub(1)),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(self.groups.len().saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::Char('a') => {
                self.all = !self.all;
                self.reload();
                return Outcome::Stay;
            }
            _ => {}
        }
        if self.selected != before {
            self.load_members();
        }
        Outcome::Stay
    }
}

// ─── Browse ──────────────────────────────────────────────────────────────────

pub(crate) struct BrowseRow {
    pub(crate) name: String,
    pub(crate) folder: Option<String>, // Some(full path) for a folder you can go into
    pub(crate) files: i64,
    pub(crate) bytes: i64,
    pub(crate) duplicated: i64, // bytes in files that have another copy in the index
}

pub(crate) struct Browse {
    pub(crate) target: Target,
    pub(crate) folder: String,
    pub(crate) rows: Vec<BrowseRow>,
    pub(crate) selected: usize,
    pub(crate) total: (i64, i64), // files, bytes in this folder and below
    pub(crate) error: Option<String>,
    trail: Vec<(String, usize)>, // folders above, with the row that was selected
}

impl Browse {
    pub(crate) fn new(target: Target) -> Self {
        let folder = target.path.clone();
        let mut b = Browse { target, folder, rows: Vec::new(), selected: 0, total: (0, 0), error: None, trail: Vec::new() };
        b.load();
        b
    }

    fn load(&mut self) {
        match self.query() {
            Ok(rows) => {
                self.total = (rows.iter().map(|r| r.files).sum(), rows.iter().map(|r| r.bytes).sum());
                self.rows = rows;
                self.error = None;
            }
            Err(e) => {
                self.rows = Vec::new();
                self.error = Some(e);
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    /// Folders directly inside the current one (with everything below them rolled up), then
    /// the files directly in it as one row; largest first
    fn query(&self) -> Result<Vec<BrowseRow>, String> {
        let conn = open(&self.target)?;
        let scope = resolve_scope(&conn, Some(&self.folder))?;
        let p = build_parts(&conn, &scope, &FilterArgs::default()).map_err(|e| e.to_string())?;
        // "Duplicated" counts files with another copy anywhere in the index, hard links once
        let sql = format!(
            "{}, dups AS (
                 SELECT checksum FROM files
                 WHERE size > 0 AND checksum IS NOT NULL AND checksum != '' GROUP BY checksum
                 HAVING COUNT(DISTINCT copy_id) > 1)
             SELECT directory, COUNT(*), COALESCE(SUM(size), 0),
                    COALESCE(SUM(CASE WHEN checksum IN (SELECT checksum FROM dups) THEN size ELSE 0 END), 0)
             FROM files{} GROUP BY directory",
            p.with,
            p.where_sql()
        );
        let mut folders: HashMap<String, BrowseRow> = HashMap::new();
        let mut here = BrowseRow { name: "(files in this folder)".into(), folder: None, files: 0, bytes: 0, duplicated: 0 };
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let params: Vec<Value> = p.params.clone();
        let mut rows = stmt.query(params_from_iter(params.iter())).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let dir: String = r.get::<_, Option<String>>(0).map_err(|e| e.to_string())?.unwrap_or_default();
            let (files, bytes, dup): (i64, i64, i64) =
                (r.get(1).map_err(|e| e.to_string())?, r.get(2).map_err(|e| e.to_string())?, r.get(3).map_err(|e| e.to_string())?);
            let row = match Path::new(&dir).strip_prefix(&self.folder).ok().and_then(|rel| rel.components().next()) {
                Some(first) => {
                    let name = first.as_os_str().to_string_lossy().into_owned();
                    let full = Path::new(&self.folder).join(&name).to_string_lossy().into_owned();
                    folders.entry(name.clone()).or_insert(BrowseRow { name, folder: Some(full), files: 0, bytes: 0, duplicated: 0 })
                }
                None => &mut here,
            };
            row.files += files;
            row.bytes += bytes;
            row.duplicated += dup;
        }
        let mut out: Vec<BrowseRow> = folders.into_values().collect();
        out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        if here.files > 0 {
            out.push(here);
        }
        Ok(out)
    }

    pub(crate) fn command(&self) -> String {
        format!("diskunder report {} --by-directory --db {}", quote(&self.folder), quote(&self.target.db))
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Back,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(folder) = self.rows.get(self.selected).and_then(|r| r.folder.clone()) {
                    self.trail.push((self.folder.clone(), self.selected));
                    self.folder = folder;
                    self.selected = 0;
                    self.load();
                }
            }
            KeyCode::Backspace | KeyCode::Left => {
                if let Some((folder, selected)) = self.trail.pop() {
                    self.folder = folder;
                    self.selected = selected;
                    self.load();
                }
            }
            _ => {}
        }
        Outcome::Stay
    }
}
