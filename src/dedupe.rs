//! `dedupe`: in each duplicate group keep one copy and quarantine the others. It previews by
//! default, from the index alone; `--apply` acts, re-checking every file on disk first. `--plan
//! FILE` writes the plan as CSV to review or edit, and `--from-plan FILE` acts on exactly that.

use rusqlite::{params_from_iter, types::Value, Connection};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use crate::blocks::Shared;
use crate::db::{has_dup_groups, lock_db, refresh_dup_groups};
use crate::fmt::{fmt_bytes, fmt_count, fmt_local};
use crate::output::{write_cells, Cell, Format, Out};
use crate::quarantine::{absolute_file, ask, move_planned, new_batch, plan, plural, relinked, Moved, Planned};
use crate::query::{absolute, build_parts, resolve_scope, ExtList, FilterArgs};
use crate::runlog::RunLog;
use crate::settings::Settings;

/// Which copy in a group stays
#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub(crate) enum Keep {
    /// The earliest modified — usually the original
    Oldest,
    /// The latest modified
    Newest,
    /// The shortest path
    ShortestPath,
    /// The first path alphabetically
    FirstPath,
}

/// What happens to the copies that don't stay
#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub(crate) enum Action {
    /// Move them to quarantine, from where they can be restored
    Quarantine,
    /// Share the kept copy's blocks, so each path stays but the data is stored once (Linux,
    /// XFS or Btrfs, run on the server); copies on another volume are quarantined
    Share,
    /// Replace them with hard links to the kept copy, after comparing them byte for byte —
    /// they then share one set of permissions and edits; copies on another volume are quarantined
    Hardlink,
}

impl Action {
    /// As typed after --action
    pub(crate) fn name(self) -> &'static str {
        match self {
            Action::Quarantine => "quarantine",
            Action::Share => "share",
            Action::Hardlink => "hardlink",
        }
    }

    /// The next action, for choosing one in the app
    pub(crate) fn next(self) -> Action {
        match self {
            Action::Quarantine => Action::Share,
            Action::Share => Action::Hardlink,
            Action::Hardlink => Action::Quarantine,
        }
    }
}

pub(crate) struct DedupeOptions<'a> {
    pub(crate) path: Option<&'a str>,
    pub(crate) keep: Keep,
    pub(crate) keep_under: &'a [String],
    pub(crate) remove_under: &'a [String],
    pub(crate) ext: Option<&'a ExtList>,
    pub(crate) min_size: Option<u64>,
    pub(crate) min_copies: usize,
    pub(crate) min_wasted: Option<u64>,
    pub(crate) action: Action,
    pub(crate) limit: usize, // groups shown in the preview; 0 = all
    pub(crate) plan: Option<&'a str>,
    pub(crate) from_plan: Option<&'a str>,
    pub(crate) apply: bool,
    pub(crate) dry_run: bool,
    pub(crate) yes: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Role {
    Keep,   // the copy that stays
    Remove, // a copy to quarantine
    Linked, // the kept copy under another name (a hard link, or another mount): removing it frees nothing
    Leave,  // a copy the options leave alone
}

impl Role {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Role::Keep => "KEEP",
            Role::Remove => "REMOVE",
            Role::Linked => "LINKED",
            Role::Leave => "LEAVE",
        }
    }
}

/// One path of a group
#[derive(Clone)]
pub(crate) struct Member {
    pub(crate) path: String,
    pub(crate) modified: String,
    pub(crate) device: Option<i64>,
    pub(crate) inode: Option<i64>,
    pub(crate) role: Role,
    pub(crate) can_go: bool, // the options allow removing it (its role may still be Keep)
}

impl Member {
    /// Whether two paths are names of one file
    pub(crate) fn same_file(&self, other: &Member) -> bool {
        self.path == other.path || self.inode.is_some() && self.device == other.device && self.inode == other.inode
    }
}

pub(crate) struct Group {
    pub(crate) checksum: String,
    pub(crate) size: i64,
    pub(crate) copies: usize, // distinct files: names of one file count once
    pub(crate) members: Vec<Member>, // the kept path first, then its other names, then the rest by path
}

impl Group {
    /// Copies to remove (hard links to one file count once) and the paths they have
    pub(crate) fn removing(&self) -> (usize, usize) {
        let paths: Vec<&Member> = self.members.iter().filter(|m| m.role == Role::Remove).collect();
        let files: HashSet<(Option<i64>, Option<i64>, &str)> = paths
            .iter()
            .map(|m| if m.inode.is_some() { (m.device, m.inode, "") } else { (None, None, m.path.as_str()) })
            .collect();
        (files.len(), paths.len())
    }
}

fn db(e: rusqlite::Error) -> String {
    e.to_string()
}

/// Whether `path` is `dir` or inside it
fn is_under(path: &str, dir: &str) -> bool {
    dir == "/" || path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

// ─── Planning, from the index ────────────────────────────────────────────────

/// (path, modified, device, inode, copy id, extension)
type Row = (String, String, Option<i64>, Option<i64>, String, String);

/// How a group's roles are chosen
pub(crate) struct Rules<'a> {
    pub(crate) keep: Keep,
    pub(crate) keep_under: Vec<String>,   // absolute
    pub(crate) remove_under: Vec<String>, // absolute
    pub(crate) ext: Option<&'a ExtList>,
    pub(crate) chosen: &'a HashMap<String, String>, // checksum → the path to keep, chosen by hand
}

impl Rules<'_> {
    fn removable(&self, path: &str, ext: &str) -> bool {
        (self.remove_under.is_empty() || self.remove_under.iter().any(|u| is_under(path, u)))
            && self.ext.is_none_or(|ExtList(list)| list.iter().any(|e| *e == ext.to_lowercase()))
    }
}

/// Every duplicate group in scope that has something to remove, most wasted space first
fn plan_groups(conn: &Connection, o: &DedupeOptions) -> Result<Vec<Group>, String> {
    let scope = resolve_scope(conn, o.path)?;
    for note in scope.freshness_notes() {
        eprintln!("{}", note);
    }
    // Only the path given: copies elsewhere neither count nor stay
    let parts = build_parts(conn, &scope, &FilterArgs::default()).map_err(db)?;
    let and_scope = parts.conds.as_ref().map_or(String::new(), |c| format!(" AND {}", c));

    let mut conds = vec!["copies >= ?".to_string()];
    let mut params: Vec<Value> = vec![Value::Integer(o.min_copies.max(2) as i64)];
    if let Some(n) = o.min_size {
        conds.push("size >= ?".into());
        params.push(Value::Integer(n as i64));
    }
    if let Some(n) = o.min_wasted {
        conds.push("size * (copies - 1) >= ?".into());
        params.push(Value::Integer(n as i64));
    }
    let (sql, mut all_params) = if scope.everything && has_dup_groups(conn).map_err(db)? {
        (format!("SELECT checksum, size FROM dup_groups WHERE {} ORDER BY size * (copies - 1) DESC, checksum", conds.join(" AND ")), vec![])
    } else {
        (
            format!(
                "{} SELECT checksum, size FROM (
                     SELECT checksum, MAX(size) AS size, COUNT(DISTINCT copy_id) AS copies FROM files
                     WHERE size > 0 AND checksum IS NOT NULL AND checksum != ''{}
                     GROUP BY checksum)
                 WHERE {} ORDER BY size * (copies - 1) DESC, checksum",
                parts.with,
                and_scope,
                conds.join(" AND ")
            ),
            parts.params.clone(),
        )
    };
    all_params.extend(params);
    let mut stmt = conn.prepare(&sql).map_err(db)?;
    let found: Vec<(String, i64)> = stmt
        .query_map(params_from_iter(all_params.iter()), |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)?;

    let none = HashMap::new();
    let rules = Rules {
        keep: o.keep,
        keep_under: o.keep_under.iter().map(|p| absolute(p)).collect(),
        remove_under: o.remove_under.iter().map(|p| absolute(p)).collect(),
        ext: o.ext,
        chosen: &none,
    };
    let mut groups = plan_found(conn, &parts, &found, &rules)?;
    groups.retain(|g| g.members.iter().any(|m| m.role == Role::Remove));
    Ok(groups)
}

/// The roles in each group found — (checksum, size) — among the copies in scope (`parts`)
fn plan_found(conn: &Connection, parts: &crate::query::Parts, found: &[(String, i64)], rules: &Rules) -> Result<Vec<Group>, String> {
    let and_scope = parts.conds.as_ref().map_or(String::new(), |c| format!(" AND {}", c));
    let mut rows: HashMap<String, Vec<Row>> = HashMap::new();
    // In chunks, to stay under SQLite's variable limit
    for chunk in found.chunks(500) {
        let sql = format!(
            "{} SELECT checksum, path, MIN(modified_date), MIN(device), MIN(inode), MIN(copy_id), MIN(extension) FROM files
             WHERE checksum IN ({}){} GROUP BY checksum, path ORDER BY checksum, path",
            parts.with,
            vec!["?"; chunk.len()].join(", "),
            and_scope
        );
        let mut values: Vec<Value> = chunk.iter().map(|(c, _)| Value::Text(c.clone())).collect();
        values.extend(parts.params.iter().cloned());
        let mut stmt = conn.prepare(&sql).map_err(db)?;
        let found = stmt
            .query_map(params_from_iter(values.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get(1)?,
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    ),
                ))
            })
            .map_err(db)?;
        for row in found {
            let (checksum, row) = row.map_err(db)?;
            rows.entry(checksum).or_default().push(row);
        }
    }
    Ok(found
        .iter()
        .filter_map(|(checksum, size)| {
            let list = rows.remove(checksum)?;
            choose(rules, checksum.clone(), *size, list)
        })
        .collect())
}

/// The roles in some duplicate groups — (checksum, size) — for the app, which lets the copy
/// to keep be chosen by hand. Groups with nothing to remove are included.
pub(crate) fn plan_for(db_path: &str, path: Option<&str>, found: &[(String, i64)], chosen: &HashMap<String, String>) -> Result<Vec<Group>, String> {
    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    let scope = resolve_scope(&conn, path)?;
    let parts = build_parts(&conn, &scope, &FilterArgs::default()).map_err(db)?;
    let rules = Rules { keep: Keep::Oldest, keep_under: Vec::new(), remove_under: Vec::new(), ext: None, chosen };
    plan_found(&conn, &parts, found, &rules)
}

/// Decide each path's role in a group; None unless it has two copies or more
fn choose(rules: &Rules, checksum: String, size: i64, rows: Vec<Row>) -> Option<Group> {
    let (keep, keep_under) = (rules.keep, &rules.keep_under);
    let chosen = rules.chosen.get(&checksum).map(String::as_str);
    // The paths of each copy: one file under several names is one copy
    let mut copies: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for row in rows {
        copies.entry(row.4.clone()).or_default().push(row);
    }
    if copies.len() < 2 {
        return None;
    }
    // A copy can go only when every name it has may be removed, and they're hard links on one
    // device — the same file through two mounts would be moved out from under the other
    let can_go = |paths: &[Row]| {
        paths.iter().all(|p| rules.removable(&p.0, &p.5)) && (paths.len() == 1 || paths.iter().all(|p| p.2.is_some() && p.2 == paths[0].2))
    };
    // The copy that stays: one chosen by hand, then one under --keep-under, then one the options
    // leave alone (so as much as allowed can go), then by --keep; ties by path
    let rank = |paths: &Vec<Row>| {
        let picked = paths.iter().any(|p| Some(p.0.as_str()) == chosen);
        let (path, modified) = (&paths[0].0, &paths[0].1); // every name has the file's dates
        let preferred = paths.iter().any(|p| keep_under.iter().any(|u| is_under(&p.0, u)));
        let undated = matches!(keep, Keep::Oldest | Keep::Newest) && modified.is_empty();
        let none = String::new();
        let (length, older, newer) = match keep {
            Keep::Oldest => (0, modified, Reverse(&none)),
            Keep::Newest => (0, &none, Reverse(modified)),
            Keep::ShortestPath => (path.chars().count(), &none, Reverse(&none)),
            Keep::FirstPath => (0, &none, Reverse(&none)),
        };
        (!picked, !preferred, can_go(paths), undated, length, older.clone(), Reverse(newer.0.clone()), path.clone())
    };
    let kept_id = copies.iter().min_by_key(|(_, p)| rank(p)).map(|(id, _)| id.clone())?;
    let mut members = Vec::new();
    for (id, paths) in &copies {
        let go = can_go(paths);
        // The name kept: the one chosen by hand, else the first
        let named = paths.iter().position(|p| Some(p.0.as_str()) == chosen).unwrap_or(0);
        for (i, (path, modified, device, inode, _, _)) in paths.iter().enumerate() {
            let role = if *id == kept_id {
                if i == named { Role::Keep } else { Role::Linked }
            } else if go {
                Role::Remove
            } else {
                Role::Leave
            };
            members.push(Member { path: path.clone(), modified: modified.clone(), device: *device, inode: *inode, role, can_go: go });
        }
    }
    members.sort_by_key(|m| (m.role != Role::Keep, m.role != Role::Linked, m.path.clone()));
    Some(Group { checksum, size, copies: copies.len(), members })
}

// ─── Plans as CSV ────────────────────────────────────────────────────────────

const PLAN_COLUMNS: [&str; 8] = ["group", "action", "path", "size", "checksum", "modified", "device", "inode"];

fn plan_csv(groups: &[Group]) -> String {
    let mut cells = Vec::new();
    for (i, g) in groups.iter().enumerate() {
        for m in &g.members {
            let iso = if m.modified.is_empty() { String::new() } else { format!("{}Z", m.modified.replacen(' ', "T", 1)) };
            cells.push(vec![
                Cell::Int(i as i64 + 1),
                Cell::Text(m.role.label().to_string()),
                Cell::Text(m.path.clone()),
                Cell::Int(g.size),
                Cell::Text(g.checksum.clone()),
                Cell::Text(iso),
                m.device.map_or(Cell::Null, Cell::Int),
                m.inode.map_or(Cell::Null, Cell::Int),
            ]);
        }
    }
    let mut out = Out::buffer();
    write_cells(&mut out, Format::Csv, &PLAN_COLUMNS, &cells);
    out.into_text()
}

/// One CSV record's fields, quotes undone; a quoted field may span lines
fn csv_records(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut records = Vec::new();
    let (mut record, mut field, mut quoted, mut chars) = (Vec::new(), String::new(), false, text.chars().peekable());
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => record.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
                any = false;
            }
            (false, c) => field.push(c),
        }
    }
    if quoted {
        return Err("a quoted field isn't closed".into());
    }
    if any {
        record.push(field);
        records.push(record);
    }
    Ok(records)
}

/// A plan to act on: per group, the paths to keep and the paths to remove
pub(crate) struct PlanGroup {
    pub(crate) checksum: String,
    pub(crate) keep: Vec<String>,
    pub(crate) remove: Vec<String>,
}

impl From<&Group> for PlanGroup {
    fn from(g: &Group) -> Self {
        let paths = |role| g.members.iter().filter(|m| m.role == role).map(|m| m.path.clone()).collect();
        PlanGroup { checksum: g.checksum.clone(), keep: paths(Role::Keep), remove: paths(Role::Remove) }
    }
}

fn read_plan(file: &str) -> Result<Vec<PlanGroup>, String> {
    let text = fs::read_to_string(file).map_err(|e| format!("{}: {}", file, e))?;
    let mut records = csv_records(&text).map_err(|e| format!("{}: {}", file, e))?.into_iter();
    let header = records.next().ok_or_else(|| format!("{}: empty", file))?;
    let col = |name: &str| header.iter().position(|h| h.trim() == name).ok_or_else(|| format!("{}: no {} column — is it a dedupe plan?", file, name));
    let (action, path, checksum) = (col("action")?, col("path")?, col("checksum")?);
    let mut groups: Vec<PlanGroup> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (n, r) in records.enumerate() {
        if r.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        let get = |i: usize| r.get(i).map(|s| s.trim().to_string()).ok_or_else(|| format!("{}: line {} is short", file, n + 2));
        let (a, p, c) = (get(action)?.to_uppercase(), r.get(path).cloned().unwrap_or_default(), get(checksum)?);
        if c.is_empty() || p.is_empty() {
            return Err(format!("{}: line {} has no path or checksum", file, n + 2));
        }
        let i = *at.entry(c.clone()).or_insert_with(|| {
            groups.push(PlanGroup { checksum: c.clone(), keep: vec![], remove: vec![] });
            groups.len() - 1
        });
        match a.as_str() {
            "KEEP" => groups[i].keep.push(p),
            "REMOVE" => groups[i].remove.push(p),
            "LINKED" | "LEAVE" => {}
            other => return Err(format!("{}: line {}: unknown action {:?} (KEEP, REMOVE, LINKED or LEAVE)", file, n + 2, other)),
        }
    }
    groups.retain(|g| !g.remove.is_empty());
    Ok(groups)
}

// ─── Checking and acting ─────────────────────────────────────────────────────

/// A file's identity on disk now
fn file_key(p: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

/// A plan re-checked: the kept copies, the copies that can go (each with the kept copy it's
/// matched with — one on its own filesystem where there is one), and notes on the rest
struct Checked {
    keepers: Vec<Planned>,
    going: Vec<(Planned, usize)>,
    notes: Vec<String>,
}

impl Checked {
    /// Whether a copy is on the same filesystem as its kept copy, so it can share its blocks
    /// or become a link to it
    fn beside(&self, f: &Planned, k: usize) -> bool {
        matches!((file_key(&f.original), file_key(&self.keepers[k].original)), (Some(a), Some(b)) if a.0 == b.0)
    }
}

/// Re-check a plan against the disk and the index: in each group a kept copy must still be as
/// indexed, and each copy to remove must be as indexed, hold the group's checksum, and not be
/// the kept file under another name
fn check(conn: &Connection, batch: &str, groups: &[PlanGroup]) -> Result<Checked, String> {
    let keep: Vec<String> = groups.iter().flat_map(|g| g.keep.iter().cloned()).collect();
    let remove: Vec<String> = groups.iter().flat_map(|g| g.remove.iter().cloned()).collect();
    let (kept, kept_skipped) = plan(conn, batch, &keep)?;
    let (going, mut notes) = plan(conn, batch, &remove)?;
    notes.extend(kept_skipped.into_iter().map(|s| format!("kept copy {}", s)));
    let mut kept: HashMap<PathBuf, Planned> = kept.into_iter().map(|p| (p.original.clone(), p)).collect();
    let mut going: HashMap<PathBuf, Planned> = going.into_iter().map(|p| (p.original.clone(), p)).collect();
    let mut c = Checked { keepers: Vec::new(), going: Vec::new(), notes };
    for g in groups {
        // This group's kept copies: (index in c.keepers, identity on disk)
        let mut mine: Vec<(usize, (u64, u64))> = Vec::new();
        for p in &g.keep {
            let Some(k) = absolute_file(p).ok().and_then(|p| kept.remove(&p)) else { continue };
            if k.checksum.as_deref() != Some(g.checksum.as_str()) {
                continue;
            }
            if let Some(key) = file_key(&k.original) {
                c.keepers.push(k);
                mine.push((c.keepers.len() - 1, key));
            }
        }
        if mine.is_empty() {
            c.notes.push(format!(
                "{} — no kept copy is still as indexed, so its copies stay (run update)",
                g.keep.first().or(g.remove.first()).map_or("a group", String::as_str)
            ));
            continue;
        }
        for p in &g.remove {
            let Some(f) = absolute_file(p).ok().and_then(|p| going.remove(&p)) else { continue };
            let shown = f.original.display().to_string();
            let key = file_key(&f.original);
            if f.checksum.as_deref() != Some(g.checksum.as_str()) {
                c.notes.push(format!("{} — not a copy of its group's kept file (checksum differs in the index)", shown));
            } else if key.is_none_or(|k| mine.iter().any(|m| m.1 == k)) {
                c.notes.push(format!("{} — the kept file itself, under another name", shown));
            } else {
                let dev = key.map(|k| k.0);
                let k = mine.iter().find(|m| Some(m.1 .0) == dev).unwrap_or(&mine[0]).0;
                c.going.push((f, k));
            }
        }
    }
    Ok(c)
}

/// What an action does to a copy on the kept copy's filesystem — None for quarantine, which
/// treats every copy the same
fn in_place_verb(action: Action) -> Option<&'static str> {
    match action {
        Action::Quarantine => None,
        Action::Share => Some("share blocks for"),
        Action::Hardlink => Some("hard-link"),
    }
}

/// "quarantine 3 files", "share blocks for 2 files and quarantine 1 on another volume"
fn doing(action: Action, paths: usize, in_place: usize) -> String {
    match in_place_verb(action) {
        Some(verb) if in_place == paths => format!("{} {}", verb, plural(paths, "file")),
        Some(verb) if in_place > 0 => {
            format!("{} {} and quarantine {} on another volume", verb, plural(in_place, "file"), fmt_count((paths - in_place) as i64))
        }
        _ => format!("quarantine {}", plural(paths, "file")),
    }
}

/// What acting on a plan did: a one-line summary, notes on each file skipped or not done
/// ("skip: path — why"), and the quarantine batch if anything went there
pub(crate) struct Acted {
    pub(crate) summary: String,
    pub(crate) notes: Vec<String>,
    pub(crate) batch: Option<String>,
}

/// Check a plan and act on it, under the database lock — the caller has asked. Logged, notes
/// included.
pub(crate) fn act(db_path: &str, groups: &[PlanGroup], action: Action, runlog: &mut RunLog) -> Result<Acted, String> {
    if action == Action::Share && !cfg!(target_os = "linux") {
        return Err("sharing blocks needs Linux: run diskunder-linux on the server that has the volume".into());
    }
    let conn = crate::quarantine::open(db_path, true)?;
    let _lock = lock_db(db_path, "diskunder dedupe")?;
    let batch = new_batch(&conn)?;
    runlog.start();
    let result = act_on(&conn, db_path, &batch, groups, action);
    match &result {
        Ok(a) => {
            for n in &a.notes {
                runlog.error(n);
            }
            runlog.end("completed", &a.summary, 0);
        }
        Err(e) => {
            runlog.error(e);
            runlog.end("failed", e, 1);
        }
    }
    result
}

fn act_on(conn: &Connection, db_path: &str, batch: &str, groups: &[PlanGroup], action: Action) -> Result<Acted, String> {
    let c = check(conn, batch, groups)?;
    let mut notes: Vec<String> = c.notes.iter().map(|n| format!("skip: {}", n)).collect();
    let (mut skipped, mut failed) = (c.notes.len(), 0usize);
    let mut done = Vec::new();
    let mut aside = Vec::new();
    let mut in_place = Vec::new();
    for (f, k) in &c.going {
        if action != Action::Quarantine && c.beside(f, *k) {
            in_place.push((f, &c.keepers[*k]));
        } else {
            aside.push(f);
        }
    }
    match action {
        Action::Quarantine => {}
        Action::Share => {
            let (mut now, mut freed, mut already, mut affected) = (0usize, 0u64, 0usize, Vec::new());
            for (f, keep) in &in_place {
                let result = crate::blocks::share(&keep.original, &f.original, f.size);
                if matches!(result, Ok(Shared::Now | Shared::Already)) {
                    crate::quarantine::shared(conn, f, keep)
                        .map_err(|e| format!("{} now shares blocks but that couldn't be recorded: {}", f.original.display(), e))?;
                    affected.extend(f.checksum.clone());
                }
                match result {
                    Ok(Shared::Now) => {
                        now += 1;
                        freed += f.size;
                    }
                    Ok(Shared::Already) => already += 1,
                    Ok(Shared::Differs) => {
                        notes.push(format!("skip: {} — the kernel found it differs from the kept copy", f.original.display()));
                        skipped += 1;
                    }
                    Err(e) => {
                        notes.push(format!("not shared: {} — {}", f.original.display(), e));
                        failed += 1;
                    }
                }
            }
            refresh_dup_groups(conn, &affected).map_err(|e| e.to_string())?;
            done.push(format!("shared blocks for {}, freeing {}", plural(now, "file"), fmt_bytes(freed)));
            if already > 0 {
                done.push(format!("{} already shared", plural(already, "file")));
            }
        }
        Action::Hardlink => {
            let (mut linked, mut freed, mut affected) = (0usize, 0u64, Vec::new());
            for (f, keep) in &in_place {
                match crate::blocks::same_bytes(&keep.original, &f.original) {
                    Ok(true) => {}
                    Ok(false) => {
                        notes.push(format!("skip: {} — differs from the kept copy, byte for byte", f.original.display()));
                        skipped += 1;
                        continue;
                    }
                    Err(e) => {
                        notes.push(format!("not linked: {} — {}", f.original.display(), e));
                        failed += 1;
                        continue;
                    }
                }
                // Its blocks shared earlier: linking frees nothing more
                let frees = if crate::blocks::already_shared(&keep.original, &f.original) { 0 } else { f.size };
                if let Err(e) = crate::blocks::link_over(&keep.original, &f.original) {
                    notes.push(format!("not linked: {} — {}", f.original.display(), e));
                    failed += 1;
                    continue;
                }
                relinked(conn, f, keep)
                    .map_err(|e| format!("{} is now a hard link but its row couldn't be updated: {} (run update)", f.original.display(), e))?;
                linked += 1;
                freed += frees;
                affected.extend(f.checksum.clone());
            }
            refresh_dup_groups(conn, &affected).map_err(|e| e.to_string())?;
            done.push(format!("replaced {} with hard links, freeing {}", plural(linked, "file"), fmt_bytes(freed)));
        }
    }
    let mut used_batch = None;
    if !aside.is_empty() {
        let moved = move_planned(conn, db_path, batch, Some("dedupe"), &aside)?;
        failed += moved.failed.len();
        notes.extend(moved.failed.iter().map(|f| format!("not moved: {}", f)));
        if moved.files > 0 {
            used_batch = Some(batch.to_string());
        }
        done.push(Moved { failed: Vec::new(), ..moved }.message(batch, 0));
    }
    if failed > 0 {
        done.push(format!("{} failed", plural(failed, "file")));
    }
    if skipped > 0 {
        done.push(format!("{} skipped", plural(skipped, "file")));
    }
    if c.going.is_empty() {
        let e = format!("nothing done — every file was skipped ({})", done.join("; "));
        // The notes say why, so they go with the error
        return Err(std::iter::once(e).chain(notes).collect::<Vec<_>>().join("\n"));
    }
    Ok(Acted { summary: format!("dedupe: {}", done.join("; ")), notes, batch: used_batch })
}

/// The command line's --apply: ask, act, print
fn apply(db_path: &str, settings: &Settings, groups: &[PlanGroup], what: &str, action: Action, yes: bool, runlog: &mut RunLog) -> Result<(), String> {
    if action == Action::Share && !cfg!(target_os = "linux") {
        return Err("--action share needs Linux: run diskunder-linux on the server that has the volume".into());
    }
    let files = plural(groups.iter().map(|g| g.remove.len()).sum(), "file");
    let (question, asked) = confirm_text(action, &files, what);
    ask(&question, yes || (!asked && !settings.yes("deletion.confirm_quarantine")))?;
    let acted = act(db_path, groups, action, runlog)?;
    for n in &acted.notes {
        eprintln!("{}", n);
    }
    println!("{}", acted.summary);
    if let Some(batch) = acted.batch {
        println!("undo the quarantine with: diskunder quarantine restore {}", batch);
    }
    Ok(())
}

/// The question before acting, and whether it's always asked (an action that can't be undone)
pub(crate) fn confirm_text(action: Action, files: &str, what: &str) -> (String, bool) {
    match action {
        Action::Quarantine => (format!("Quarantine {} from {}? Each is checked first; they can be restored later.", files, what), false),
        Action::Share => (
            format!("Share blocks for {} from {} with their kept copies? Each is checked first; copies on another volume are quarantined.", files, what),
            false,
        ),
        Action::Hardlink => (
            format!(
                "Replace {} from {} with hard links to their kept copies? Each is compared byte for byte first, but this can't be undone; copies on another volume are quarantined.",
                files, what
            ),
            true,
        ),
    }
}

// ─── The command ─────────────────────────────────────────────────────────────

fn name_of(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// A copy on the kept copy's device, as indexed — where `share` and `hardlink` act in place
fn beside_keeper(g: &Group, m: &Member) -> bool {
    let keeper = g.members.iter().find(|k| k.role == Role::Keep);
    m.device.is_some() && keeper.is_some_and(|k| k.device == m.device)
}

fn label(action: Action, g: &Group, m: &Member) -> &'static str {
    match (m.role, action) {
        (Role::Remove, Action::Share) if beside_keeper(g, m) => "SHARE",
        (Role::Remove, Action::Hardlink) if beside_keeper(g, m) => "LINK",
        (role, _) => role.label(),
    }
}

fn preview(groups: &[Group], limit: usize, action: Action) {
    let shown = if limit == 0 { groups.len() } else { limit.min(groups.len()) };
    let mut out = Out::new();
    for g in &groups[..shown] {
        out.write(&format!("{} — {} × {} copies\n", name_of(&g.members[0].path), fmt_bytes(g.size as u64), g.copies));
        for m in &g.members {
            let when = if m.modified.is_empty() { "-".to_string() } else { fmt_local(&m.modified) };
            out.write(&format!("  {:<6}  {:<16}  {}\n", label(action, g, m), when, m.path));
        }
        out.write("\n");
    }
    if shown < groups.len() {
        out.write(&format!("… and {} more (--limit 0 lists them all, or --plan FILE)\n\n", plural(groups.len() - shown, "group")));
    }
    out.flush();
}

/// "Would quarantine 3 files from 2 groups, freeing 1.2 GB"
fn would(groups: &[Group], action: Action) -> String {
    let (mut files, mut paths, mut in_place, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    for g in groups {
        let (f, p) = g.removing();
        files += f;
        paths += p;
        bytes += f as u64 * g.size as u64;
        in_place += g.members.iter().filter(|m| m.role == Role::Remove && beside_keeper(g, m)).count();
    }
    let links = if paths > files { format!(" ({} copies; the rest are hard links to them)", fmt_count(files as i64)) } else { String::new() };
    format!("Would {}{} from {}, freeing {}", doing(action, paths, in_place), links, plural(groups.len(), "group"), fmt_bytes(bytes))
}

fn dedupe(db_path: &str, o: &DedupeOptions, settings: &Settings, runlog: &mut RunLog) -> Result<(), String> {
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    // Acting is the default only when deletion.dry_run_default is no
    let act = o.apply || (!o.dry_run && !settings.yes("deletion.dry_run_default"));

    if let Some(file) = o.from_plan {
        let groups = read_plan(file)?;
        if groups.is_empty() {
            println!("nothing to remove in {}", file);
            return Ok(());
        }
        let what = format!("{} in {}", plural(groups.len(), "group"), file);
        if act {
            return apply(db_path, settings, &groups, &what, o.action, o.yes, runlog);
        }
        let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
        let c = check(&conn, "preview", &groups)?;
        for n in &c.notes {
            eprintln!("skip: {}", n);
        }
        let mut in_place = 0;
        for (f, k) in &c.going {
            let here = in_place_verb(o.action).is_some() && c.beside(f, *k);
            in_place += here as usize;
            let tag = match (here, o.action) {
                (true, Action::Share) => "SHARE",
                (true, _) => "LINK",
                _ => "REMOVE",
            };
            println!("  {:<6}  {}", tag, f.original.display());
        }
        let bytes: u64 = c.going.iter().map(|(f, _)| f.size).sum();
        println!(
            "\nWould {} ({}) from {}, each checked just now (preview — nothing was changed; add --apply to act)",
            doing(o.action, c.going.len(), in_place),
            fmt_bytes(bytes),
            what
        );
        return Ok(());
    }

    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    let groups = plan_groups(&conn, o)?;
    let to_stdout = o.plan == Some("-");
    if let Some(file) = o.plan {
        let csv = plan_csv(&groups);
        if to_stdout {
            print!("{}", csv);
        } else {
            fs::write(file, csv).map_err(|e| format!("{}: {}", file, e))?;
        }
    }
    if groups.is_empty() {
        eprintln!("no duplicates to remove");
        return Ok(());
    }
    if act {
        if !to_stdout {
            preview(&groups, o.limit, o.action);
        }
        let plan: Vec<PlanGroup> = groups.iter().map(PlanGroup::from).collect();
        drop(conn);
        return apply(db_path, settings, &plan, &plural(groups.len(), "group"), o.action, o.yes, runlog);
    }
    if to_stdout {
        eprintln!("{} (preview — nothing was changed)", would(&groups, o.action));
        return Ok(());
    }
    preview(&groups, o.limit, o.action);
    if let Some(file) = o.plan {
        println!("Plan written to {} — edit it, then: diskunder dedupe --from-plan {}", file, file);
    }
    println!("{} (preview — nothing was changed; add --apply to act)", would(&groups, o.action));
    Ok(())
}

pub(crate) fn run_dedupe(db_path: &str, o: &DedupeOptions, settings: &Settings, runlog: &mut RunLog) -> i32 {
    match dedupe(db_path, o, settings, runlog) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, modified: &str, id: &str) -> Row {
        (path.into(), modified.into(), Some(1), Some(id.len() as i64), id.into(), "mov".into())
    }

    /// The group's roles, or None when nothing would be removed
    fn plan(keep: Keep, keep_under: &[&str], remove_under: &[&str], chosen: Option<&str>, rows: Vec<Row>) -> Option<Group> {
        let chosen: HashMap<String, String> = chosen.map(|p| ("c".to_string(), p.to_string())).into_iter().collect();
        let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        let rules = Rules { keep, keep_under: strings(keep_under), remove_under: strings(remove_under), ext: None, chosen: &chosen };
        choose(&rules, "c".into(), 10, rows).filter(|g| g.members.iter().any(|m| m.role == Role::Remove))
    }

    fn roles(g: &Group) -> Vec<(&str, &str)> {
        g.members.iter().map(|m| (m.role.label(), m.path.as_str())).collect()
    }

    #[test]
    fn keeping_the_right_copy() {
        let rows = || vec![row("/n/b/x", "2024-01-01 00:00:00", "a"), row("/n/a/longer/x", "2023-01-01 00:00:00", "bb"), row("/n/c/x", "2025-01-01 00:00:00", "ccc")];
        let pick = |keep, under: &[&str]| plan(keep, under, &[], None, rows()).unwrap().members[0].path.clone();
        assert_eq!(pick(Keep::Oldest, &[]), "/n/a/longer/x");
        assert_eq!(pick(Keep::Newest, &[]), "/n/c/x");
        assert_eq!(pick(Keep::ShortestPath, &[]), "/n/b/x");
        assert_eq!(pick(Keep::FirstPath, &[]), "/n/a/longer/x");
        assert_eq!(pick(Keep::Oldest, &["/n/c"]), "/n/c/x");
        assert_eq!(plan(Keep::Oldest, &["/n/c"], &[], Some("/n/b/x"), rows()).unwrap().members[0].path, "/n/b/x", "chosen by hand beats everything");
        // Only /n/b may go: the kept copy is one that would stay anyway
        let g = plan(Keep::Newest, &[], &["/n/b"], None, rows()).unwrap();
        assert_eq!(roles(&g), [("KEEP", "/n/c/x"), ("LEAVE", "/n/a/longer/x"), ("REMOVE", "/n/b/x")]);
        let g = plan(Keep::Newest, &[], &["/n/c"], None, rows()).unwrap();
        assert_eq!(g.members[0].path, "/n/b/x", "newest of the copies that can't go");
        assert!(plan(Keep::Oldest, &[], &["/nowhere"], None, rows()).is_none());
    }

    #[test]
    fn hard_links_go_together_or_not_at_all() {
        let mut rows = vec![row("/n/a/x", "2023-01-01 00:00:00", "a"), row("/n/b/x", "2024-01-01 00:00:00", "bb"), row("/n/c/x", "2024-01-01 00:00:00", "bb")];
        let g = plan(Keep::Oldest, &[], &[], None, rows.clone()).unwrap();
        assert_eq!(roles(&g), [("KEEP", "/n/a/x"), ("REMOVE", "/n/b/x"), ("REMOVE", "/n/c/x")]);
        assert_eq!(g.removing(), (1, 2));
        // Only one name under --remove-under: removing it would free nothing
        assert!(plan(Keep::Oldest, &[], &["/n/b"], None, rows.clone()).is_none());
        // Keeping a copy chosen by name keeps that name, its other names linked
        let g = plan(Keep::Oldest, &[], &[], Some("/n/c/x"), rows.clone()).unwrap();
        assert_eq!(roles(&g), [("KEEP", "/n/c/x"), ("LINKED", "/n/b/x"), ("REMOVE", "/n/a/x")]);
        // A link to the kept copy is never removed
        rows.push(row("/n/d/x", "2023-01-01 00:00:00", "a"));
        let g = plan(Keep::Oldest, &[], &[], None, rows.clone()).unwrap();
        assert_eq!(roles(&g)[1], ("LINKED", "/n/d/x"));
        // The same file through two mounts (other devices) can't go, so it's the one kept
        rows[2].2 = Some(2);
        let g = plan(Keep::Oldest, &[], &[], None, rows).unwrap();
        assert_eq!(roles(&g), [("KEEP", "/n/b/x"), ("LINKED", "/n/c/x"), ("REMOVE", "/n/a/x"), ("REMOVE", "/n/d/x")]);
    }

    #[test]
    fn csv_round_trip() {
        let text = "a,b,c\n1,\"x, \"\"y\"\"\nz\",3\r\n4,5,6";
        assert_eq!(
            csv_records(text).unwrap(),
            vec![vec!["a", "b", "c"], vec!["1", "x, \"y\"\nz", "3"], vec!["4", "5", "6"]]
        );
        assert!(csv_records("a,\"b").is_err());
    }
}
