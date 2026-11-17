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

use crate::db::{has_dup_groups, lock_db};
use crate::fmt::{fmt_bytes, fmt_count, fmt_local};
use crate::output::{write_cells, Cell, Format, Out};
use crate::quarantine::{absolute_file, ask, move_planned, new_batch, plan, plural, Planned};
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

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Keep,   // the copy that stays
    Remove, // a copy to quarantine
    Linked, // the kept copy under another name (a hard link, or another mount): removing it frees nothing
    Leave,  // a copy the options leave alone
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Keep => "KEEP",
            Role::Remove => "REMOVE",
            Role::Linked => "LINKED",
            Role::Leave => "LEAVE",
        }
    }
}

/// One path of a group
struct Member {
    path: String,
    modified: String,
    device: Option<i64>,
    inode: Option<i64>,
    role: Role,
}

struct Group {
    checksum: String,
    size: i64,
    copies: usize, // distinct files: names of one file count once
    members: Vec<Member>,
}

impl Group {
    /// Copies to remove (hard links to one file count once) and the paths they have
    fn removing(&self) -> (usize, usize) {
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

    let keep_under: Vec<String> = o.keep_under.iter().map(|p| absolute(p)).collect();
    let remove_under: Vec<String> = o.remove_under.iter().map(|p| absolute(p)).collect();
    let removable = |path: &str, ext: &str| {
        (remove_under.is_empty() || remove_under.iter().any(|u| is_under(path, u)))
            && o.ext.is_none_or(|ExtList(list)| list.iter().any(|e| *e == ext.to_lowercase()))
    };
    let mut groups = Vec::new();
    for (checksum, size) in found {
        let Some(list) = rows.remove(&checksum) else { continue };
        if let Some(g) = choose(o.keep, &keep_under, &removable, checksum, size, list) {
            groups.push(g);
        }
    }
    Ok(groups)
}

/// Decide each path's role in a group; None when there's nothing to remove
fn choose(keep: Keep, keep_under: &[String], removable: &dyn Fn(&str, &str) -> bool, checksum: String, size: i64, rows: Vec<Row>) -> Option<Group> {
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
        paths.iter().all(|p| removable(&p.0, &p.5)) && (paths.len() == 1 || paths.iter().all(|p| p.2.is_some() && p.2 == paths[0].2))
    };
    // The copy that stays: one under --keep-under, then one the options leave alone (so as much
    // as allowed can go), then by --keep; ties by path
    let rank = |paths: &Vec<Row>| {
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
        (!preferred, can_go(paths), undated, length, older.clone(), Reverse(newer.0.clone()), path.clone())
    };
    let kept_id = copies.iter().min_by_key(|(_, p)| rank(p)).map(|(id, _)| id.clone())?;
    let mut members = Vec::new();
    for (id, paths) in &copies {
        let go = can_go(paths);
        for (i, (path, modified, device, inode, _, _)) in paths.iter().enumerate() {
            let role = if *id == kept_id {
                if i == 0 { Role::Keep } else { Role::Linked }
            } else if go {
                Role::Remove
            } else {
                Role::Leave
            };
            members.push(Member { path: path.clone(), modified: modified.clone(), device: *device, inode: *inode, role });
        }
    }
    members.sort_by_key(|m| (m.role != Role::Keep, m.role != Role::Linked, m.path.clone()));
    members.iter().any(|m| m.role == Role::Remove).then_some(Group { checksum, size, copies: copies.len(), members })
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
struct PlanGroup {
    checksum: String,
    keep: Vec<String>,
    remove: Vec<String>,
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

/// Re-check a plan against the disk and the index: in each group a kept copy must still be as
/// indexed, and each copy to remove must be as indexed, hold the group's checksum, and not be
/// the kept file under another name. The files that can go, and notes on the rest.
fn check(conn: &Connection, batch: &str, groups: &[PlanGroup]) -> Result<(Vec<Planned>, Vec<String>), String> {
    let keep: Vec<String> = groups.iter().flat_map(|g| g.keep.iter().cloned()).collect();
    let remove: Vec<String> = groups.iter().flat_map(|g| g.remove.iter().cloned()).collect();
    let (kept, kept_skipped) = plan(conn, batch, &keep)?;
    let (going, mut notes) = plan(conn, batch, &remove)?;
    notes.extend(kept_skipped.into_iter().map(|s| format!("kept copy {}", s)));
    let kept: HashMap<PathBuf, Planned> = kept.into_iter().map(|p| (p.original.clone(), p)).collect();
    let mut going: HashMap<PathBuf, Planned> = going.into_iter().map(|p| (p.original.clone(), p)).collect();
    let mut ok = Vec::new();
    for g in groups {
        let keys: HashSet<(u64, u64)> = g
            .keep
            .iter()
            .filter_map(|p| absolute_file(p).ok())
            .filter_map(|p| kept.get(&p))
            .filter(|k| k.checksum.as_deref() == Some(g.checksum.as_str()))
            .filter_map(|k| file_key(&k.original))
            .collect();
        if keys.is_empty() {
            notes.push(format!(
                "{} — no kept copy is still as indexed, so its copies stay (run update)",
                g.keep.first().or(g.remove.first()).map_or("a group", String::as_str)
            ));
            continue;
        }
        for p in &g.remove {
            let Some(f) = absolute_file(p).ok().and_then(|p| going.remove(&p)) else { continue };
            let shown = f.original.display().to_string();
            if f.checksum.as_deref() != Some(g.checksum.as_str()) {
                notes.push(format!("{} — not a copy of its group's kept file (checksum differs in the index)", shown));
            } else if file_key(&f.original).is_none_or(|k| keys.contains(&k)) {
                notes.push(format!("{} — the kept file itself, under another name", shown));
            } else {
                ok.push(f);
            }
        }
    }
    Ok((ok, notes))
}

/// Act on a plan: ask, then check and quarantine under the database lock
fn apply(db_path: &str, settings: &Settings, groups: &[PlanGroup], what: &str, yes: bool, runlog: &mut RunLog) -> Result<(), String> {
    let files: usize = groups.iter().map(|g| g.remove.len()).sum();
    ask(
        &format!("Quarantine {} from {}? Each is checked first; they can be restored later.", plural(files, "file"), what),
        yes || !settings.yes("deletion.confirm_quarantine"),
    )?;
    let conn = crate::quarantine::open(db_path, true)?;
    let _lock = lock_db(db_path, "diskunder dedupe")?;
    let batch = new_batch(&conn)?;
    let result = crate::maintenance::logged(runlog, || {
        let (going, notes) = check(&conn, &batch, groups)?;
        for n in &notes {
            eprintln!("skip: {}", n);
        }
        if going.is_empty() {
            return Err("nothing quarantined — every file was skipped".into());
        }
        let done = move_planned(&conn, db_path, &batch, Some("dedupe"), &going)?;
        Ok(format!("dedupe: {}", done.message(&batch, notes.len())))
    })?;
    println!("{}", result);
    println!("undo with: diskunder quarantine restore {}", batch);
    Ok(())
}

// ─── The command ─────────────────────────────────────────────────────────────

fn name_of(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

fn preview(groups: &[Group], limit: usize) {
    let shown = if limit == 0 { groups.len() } else { limit.min(groups.len()) };
    let mut out = Out::new();
    for g in &groups[..shown] {
        out.write(&format!("{} — {} × {} copies\n", name_of(&g.members[0].path), fmt_bytes(g.size as u64), g.copies));
        for m in &g.members {
            let when = if m.modified.is_empty() { "-".to_string() } else { fmt_local(&m.modified) };
            out.write(&format!("  {:<6}  {:<16}  {}\n", m.role.label(), when, m.path));
        }
        out.write("\n");
    }
    if shown < groups.len() {
        out.write(&format!("… and {} more (--limit 0 lists them all, or --plan FILE)\n\n", plural(groups.len() - shown, "group")));
    }
    out.flush();
}

fn summary(groups: &[Group]) -> (usize, usize, u64) {
    groups.iter().fold((0, 0, 0), |(f, p, b), g| {
        let (files, paths) = g.removing();
        (f + files, p + paths, b + files as u64 * g.size as u64)
    })
}

fn dedupe(db_path: &str, o: &DedupeOptions, settings: &Settings, runlog: &mut RunLog) -> Result<(), String> {
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    // Acting is the default only when deletion.dry_run_default is no
    let act = o.apply || (!o.dry_run && !settings.yes("deletion.dry_run_default"));
    let Action::Quarantine = o.action;

    if let Some(file) = o.from_plan {
        let groups = read_plan(file)?;
        if groups.is_empty() {
            println!("nothing to remove in {}", file);
            return Ok(());
        }
        let what = format!("{} in {}", plural(groups.len(), "group"), file);
        if act {
            return apply(db_path, settings, &groups, &what, o.yes, runlog);
        }
        let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
        let (going, notes) = check(&conn, "preview", &groups)?;
        for n in &notes {
            eprintln!("skip: {}", n);
        }
        for f in &going {
            println!("  {}", f.original.display());
        }
        let bytes: u64 = going.iter().map(|f| f.size).sum();
        println!(
            "\nWould quarantine {} ({}) from {}, each checked just now (preview — nothing was changed; add --apply to act)",
            plural(going.len(), "file"),
            fmt_bytes(bytes),
            what
        );
        return Ok(());
    }

    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    let groups = plan_groups(&conn, o)?;
    let (files, paths, bytes) = summary(&groups);
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
    let links = if paths > files { format!(" ({} copies; the rest are hard links to them)", fmt_count(files as i64)) } else { String::new() };
    let freeing = format!("{}{} from {}, freeing {}", plural(paths, "file"), links, plural(groups.len(), "group"), fmt_bytes(bytes));
    if act {
        if !to_stdout {
            preview(&groups, o.limit);
        }
        let plan: Vec<PlanGroup> = groups
            .iter()
            .map(|g| PlanGroup {
                checksum: g.checksum.clone(),
                keep: g.members.iter().filter(|m| m.role == Role::Keep).map(|m| m.path.clone()).collect(),
                remove: g.members.iter().filter(|m| m.role == Role::Remove).map(|m| m.path.clone()).collect(),
            })
            .collect();
        drop(conn);
        return apply(db_path, settings, &plan, &plural(groups.len(), "group"), o.yes, runlog);
    }
    if to_stdout {
        eprintln!("Would quarantine {} (preview — nothing was changed)", freeing);
        return Ok(());
    }
    preview(&groups, o.limit);
    if let Some(file) = o.plan {
        println!("Plan written to {} — edit it, then: diskunder dedupe --from-plan {}", file, file);
    }
    println!("Would quarantine {} (preview — nothing was changed; add --apply to act)", freeing);
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

    fn roles(g: &Group) -> Vec<(&str, &str)> {
        g.members.iter().map(|m| (m.role.label(), m.path.as_str())).collect()
    }

    #[test]
    fn keeping_the_right_copy() {
        let rows = || vec![row("/n/b/x", "2024-01-01 00:00:00", "a"), row("/n/a/longer/x", "2023-01-01 00:00:00", "bb"), row("/n/c/x", "2025-01-01 00:00:00", "ccc")];
        let any = |_: &str, _: &str| true;
        let pick = |keep, under: &[String]| choose(keep, under, &any, "c".into(), 10, rows()).unwrap().members[0].path.clone();
        assert_eq!(pick(Keep::Oldest, &[]), "/n/a/longer/x");
        assert_eq!(pick(Keep::Newest, &[]), "/n/c/x");
        assert_eq!(pick(Keep::ShortestPath, &[]), "/n/b/x");
        assert_eq!(pick(Keep::FirstPath, &[]), "/n/a/longer/x");
        assert_eq!(pick(Keep::Oldest, &["/n/c".to_string()]), "/n/c/x");
        // Only /n/b may go: the kept copy is one that would stay anyway
        let only_b = |p: &str, _: &str| p.starts_with("/n/b/");
        let g = choose(Keep::Newest, &[], &only_b, "c".into(), 10, rows()).unwrap();
        assert_eq!(roles(&g), [("KEEP", "/n/c/x"), ("LEAVE", "/n/a/longer/x"), ("REMOVE", "/n/b/x")]);
        let only_c = |p: &str, _: &str| p.starts_with("/n/c/");
        let g = choose(Keep::Newest, &[], &only_c, "c".into(), 10, rows()).unwrap();
        assert_eq!(g.members[0].path, "/n/b/x", "newest of the copies that can't go");
        assert!(choose(Keep::Oldest, &[], &|_: &str, _: &str| false, "c".into(), 10, rows()).is_none());
    }

    #[test]
    fn hard_links_go_together_or_not_at_all() {
        let mut rows = vec![row("/n/a/x", "2023-01-01 00:00:00", "a"), row("/n/b/x", "2024-01-01 00:00:00", "bb"), row("/n/c/x", "2024-01-01 00:00:00", "bb")];
        let any = |_: &str, _: &str| true;
        let g = choose(Keep::Oldest, &[], &any, "c".into(), 10, rows.clone()).unwrap();
        assert_eq!(roles(&g), [("KEEP", "/n/a/x"), ("REMOVE", "/n/b/x"), ("REMOVE", "/n/c/x")]);
        assert_eq!(g.removing(), (1, 2));
        // Only one name under --remove-under: removing it would free nothing
        let only_b = |p: &str, _: &str| p.starts_with("/n/b/");
        assert!(choose(Keep::Oldest, &[], &only_b, "c".into(), 10, rows.clone()).is_none());
        // A link to the kept copy is never removed
        rows.push(row("/n/d/x", "2023-01-01 00:00:00", "a"));
        let g = choose(Keep::Oldest, &[], &any, "c".into(), 10, rows.clone()).unwrap();
        assert_eq!(roles(&g)[1], ("LINKED", "/n/d/x"));
        // The same file through two mounts (other devices) can't go, so it's the one kept
        rows[2].2 = Some(2);
        let g = choose(Keep::Oldest, &[], &any, "c".into(), 10, rows).unwrap();
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
