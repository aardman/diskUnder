//! Shared query layer for `search` and `export`: filter flags, which indexes a path refers to,
//! and the SQL that selects matching files (always with bound parameters)

use chrono::{Local, NaiveDate, TimeZone, Utc};
use clap::Args;
use rusqlite::{params_from_iter, types::Value, Connection};
use std::{collections::HashMap, fs};

use crate::db::{file_tables, has_column, load_roots, schema_version, table_exists, table_name_for, RootRow, has_dup_groups};
use crate::fmt::fmt_local;

#[derive(Args, Clone, Default)]
pub(crate) struct FilterArgs {
    /// File name pattern, e.g. "*.log" (* ? and [..] wildcards)
    #[arg(long, value_name = "GLOB")]
    pub(crate) name: Option<String>,

    /// Match --name regardless of case
    #[arg(long)]
    pub(crate) ignore_case: bool,

    /// Extensions, comma-separated, e.g. jpg,mov ("none" for files without one)
    #[arg(long, value_name = "LIST", value_parser = parse_ext_list)]
    pub(crate) ext: Option<ExtList>,

    /// Only files larger than SIZE, e.g. 500MB, 1.5GB, 200K (1024-based)
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub(crate) larger_than: Option<u64>,

    /// Only files smaller than SIZE
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub(crate) smaller_than: Option<u64>,

    /// Only files last modified before AGE ago, e.g. 90d, 6w, 3m, 1y, or a date 2024-01-01
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub(crate) older_than: Option<String>,

    /// Only files modified within AGE
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub(crate) newer_than: Option<String>,

    /// Only files not accessed within AGE (access times are unreliable on many servers)
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub(crate) not_accessed: Option<String>,

    /// Only files under this folder
    #[arg(long, value_name = "PATH")]
    pub(crate) under: Option<String>,

    /// Only files with at least one other copy (hard links don't count)
    #[arg(long)]
    pub(crate) duplicate: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum SortKey {
    /// Alphabetical by full path
    Path,
    /// Largest first
    Size,
    /// Most recently modified first
    Modified,
    /// Alphabetical by file name
    Name,
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

/// "500MB", "1.5GB", "200K", "4096" → bytes. Units are 1024-based, matching the tool's output.
pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("'{}' isn't a size — try 500MB, 1.5GB or 200K", s))?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        _ => return Err(format!("unknown size unit in '{}' — use B, K, M, G or T", s)),
    };
    Ok((n * mult as f64).round() as u64)
}

/// "90d", "6w", "3m", "1y" (a month is 30 days, a year 365) or a date "2024-01-01" (local
/// midnight) → the cutoff as a stored UTC timestamp, "YYYY-MM-DD HH:MM:SS"
pub(crate) fn parse_age(s: &str) -> Result<String, String> {
    let t = s.trim();
    let cutoff = if let Ok(date) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).unwrap();
        Local
            .from_local_datetime(&midnight)
            .earliest()
            .ok_or_else(|| format!("'{}' isn't a valid local date", s))?
            .with_timezone(&Utc)
    } else {
        let (num, unit) = t.split_at(t.len().saturating_sub(1));
        let n: i64 = num.parse().map_err(|_| format!("'{}' isn't an age — try 90d, 6w, 3m, 1y or 2024-01-01", s))?;
        let days = match unit {
            "d" => n,
            "w" => n * 7,
            "m" => n * 30,
            "y" => n * 365,
            _ => return Err(format!("unknown age unit in '{}' — use d, w, m or y", s)),
        };
        Utc::now() - chrono::Duration::days(days)
    };
    Ok(cutoff.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// Lowercase extensions, sorted and de-duplicated. A newtype so clap treats the whole
/// comma-separated list as one value rather than a repeatable option.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExtList(pub(crate) Vec<String>);

/// "JPG, .mov,none" → ["jpg", "mov", ""] ("" matches files without an extension)
pub(crate) fn parse_ext_list(s: &str) -> Result<ExtList, String> {
    let mut exts: Vec<String> = s
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .map(|e| if e == "none" { String::new() } else { e })
        .collect();
    exts.sort();
    exts.dedup();
    if exts.is_empty() {
        return Err("give at least one extension, e.g. jpg,mov".to_string());
    }
    Ok(ExtList(exts))
}

/// Absolute path with no trailing slash, without requiring it to exist
pub(crate) fn absolute(p: &str) -> String {
    let abs = fs::canonicalize(p)
        .or_else(|_| std::path::absolute(p))
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string());
    match abs.trim_end_matches('/') {
        "" => "/".to_string(),
        trimmed => trimmed.to_string(),
    }
}

// ─── Which indexes ───────────────────────────────────────────────────────────

/// The index tables a command reads, and an implied `--under` when the path given was a
/// folder inside an indexed path rather than the indexed path itself
pub(crate) struct Scope {
    pub(crate) tables: Vec<String>,
    pub(crate) under: Option<String>,
    roots: HashMap<String, RootRow>,
    pub(crate) everything: bool, // every indexed path in the database (no path given)
}

pub(crate) fn resolve_scope(conn: &Connection, path: Option<&str>) -> Result<Scope, String> {
    let db = |e: rusqlite::Error| e.to_string();
    let roots = load_roots(conn).map_err(db)?;
    let Some(p) = path else {
        let tables = file_tables(conn).map_err(db)?;
        if tables.is_empty() {
            return Err("nothing is indexed in this database yet".to_string());
        }
        return Ok(Scope { tables, under: None, roots, everything: true });
    };

    let abs = absolute(p);
    if let Some((t, _)) = roots.iter().find(|(_, r)| r.root == abs) {
        return Ok(Scope { tables: vec![t.clone()], under: None, roots, everything: false });
    }
    // A folder inside an indexed path: search that index, limited to the folder
    let containing = roots
        .iter()
        .filter(|(_, r)| r.root == "/" || abs.starts_with(&format!("{}/", r.root)))
        .max_by_key(|(_, r)| r.root.len());
    if let Some((t, _)) = containing {
        return Ok(Scope { tables: vec![t.clone()], under: Some(abs), roots, everything: false });
    }
    let t = table_name_for(&abs);
    if table_exists(conn, &t).map_err(db)? {
        return Ok(Scope { tables: vec![t], under: None, roots, everything: false });
    }
    Err(format!("no index covers {} — see `diskunder db info`", abs))
}

impl Scope {
    /// Notes about how current the index is — for stderr, never mixed into the data
    pub(crate) fn freshness_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let mut finished: Vec<&str> = Vec::new();
        for t in &self.tables {
            match self.roots.get(t) {
                Some(r) => {
                    match &r.last_finished {
                        Some(end) => finished.push(end),
                        None => notes.push(format!(
                            "warning: the last run of {} did not finish — results may be incomplete",
                            r.root
                        )),
                    }
                    // So "no duplicates" isn't mistaken for "no duplicates among types not indexed"
                    if !r.filter.types.is_empty() {
                        notes.push(format!(
                            "note: {} is indexed with file-type filters ({}) — other types aren't in the index",
                            r.root,
                            r.filter.types.describe()
                        ));
                    }
                    if !r.filter.paths.is_empty() {
                        notes.push(format!(
                            "note: {} is indexed with path filters ({}) — matching paths aren't in the index",
                            r.root,
                            r.filter.paths.describe()
                        ));
                    }
                }
                None => notes.push(format!("note: {} has no recorded runs (see `diskunder db info`)", t)),
            }
        }
        if let Some(oldest) = finished.iter().min() {
            let when = fmt_local(oldest);
            notes.insert(
                0,
                if self.tables.len() == 1 {
                    format!("index as of {}", when)
                } else {
                    format!("{} indexed paths; oldest index from {}", self.tables.len(), when)
                },
            );
        }
        notes
    }

    fn root_label(&self, t: &str) -> String {
        self.roots.get(t).map_or_else(|| t.to_string(), |r| r.root.clone())
    }
}

// ─── SQL ─────────────────────────────────────────────────────────────────────

/// The columns every query produces, in this order
pub(crate) const COLUMNS: [&str; 12] = [
    "root", "path", "filename", "directory", "extension", "size", "modified", "accessed",
    "created", "checksum", "device", "inode",
];

pub(crate) struct Query {
    pub(crate) sql: String,
    pub(crate) count_sql: String, // matches, counted up to COUNT_CAP
    pub(crate) params: Vec<Value>,
}

/// Duplicate groups listed in a query rather than looked up by a subquery (see `parts_for`)
const LISTED_GROUPS: usize = 2_000;

/// Counting stops here: "… more than 100,000 more" is as useful as an exact count, and
/// counting millions of matches takes minutes on a big index
pub(crate) const COUNT_CAP: usize = 100_000;

/// What makes two rows the same file, so it counts as one copy. With a known volume: the volume
/// and inode — the same however the volume is mounted, and hard links share it. Without one: the
/// device and inode, as on the machine that indexed it. Without an inode: the path, which also
/// counts a file once when overlapping indexed paths both hold it.
fn copy_id_sql(volume: Option<&str>, has_inode: bool) -> String {
    copy_id_expr("", volume, has_inode)
}

/// `copy_id_sql` for a table aliased `alias` (e.g. "x."; "" for none)
pub(crate) fn copy_id_expr(alias: &str, volume: Option<&str>, has_inode: bool) -> String {
    let a = alias;
    match (volume, has_inode) {
        (_, false) => format!("'p:' || {a}path"),
        (Some(v), true) => format!(
            "CASE WHEN {a}inode IS NOT NULL THEN {} || {a}inode ELSE 'p:' || {a}path END",
            sql_literal(&format!("{}#", v))
        ),
        (None, true) => format!("CASE WHEN {a}inode IS NOT NULL THEN {a}device || ':' || {a}inode ELSE 'p:' || {a}path END"),
    }
}

fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One SELECT per table, normalised to COLUMNS, so older tables (no inode columns,
/// `change_date`, mixed-case extensions before schema 1) read the same as current ones
fn source_select(conn: &Connection, scope: &Scope, t: &str, lowercase_ext: bool) -> rusqlite::Result<String> {
    let birth = if has_column(conn, t, "birth_date")? {
        "birth_date"
    } else if has_column(conn, t, "change_date")? {
        "change_date"
    } else {
        "NULL"
    };
    let has_inode = has_column(conn, t, "inode")?;
    let (device, inode) = if has_inode { ("device", "inode") } else { ("NULL", "NULL") };
    let ext = if lowercase_ext { "lower(extension)" } else { "extension" };
    Ok(format!(
        "SELECT {root} AS root, path, filename, directory, {ext} AS extension, size, \
         modified_date, access_date, {birth} AS birth_date, checksum, {device} AS device, {inode} AS inode, \
         {copy_id} AS copy_id \
         FROM {t}",
        root = sql_literal(&scope.root_label(t)),
        copy_id = copy_id_sql(scope.roots.get(t).and_then(|r| r.volume.as_ref()).map(|v| v.id.as_str()), has_inode),
    ))
}

/// The pieces every query is built from: a `files` CTE over the tables in scope, and the
/// filter conditions with their bound parameters
pub(crate) struct Parts {
    pub(crate) with: String,             // "WITH files AS …"
    pub(crate) conds: Option<String>,    // "a AND b …", or None when nothing is filtered
    pub(crate) params: Vec<Value>,
}

impl Parts {
    pub(crate) fn where_sql(&self) -> String {
        self.conds.as_ref().map_or(String::new(), |c| format!(" WHERE {}", c))
    }
}

pub(crate) fn build_query(
    conn: &Connection,
    scope: &Scope,
    filters: &FilterArgs,
    sort: SortKey,
    limit: Option<usize>,
) -> rusqlite::Result<Query> {
    let Parts { with, conds, params } = build_parts(conn, scope, filters)?;
    let mut where_sql = conds.map_or(String::new(), |c| format!(" WHERE {}", c));
    let (order, column) = match sort {
        SortKey::Path => ("path", "path"),
        SortKey::Size => ("size DESC, path", "size"),
        SortKey::Modified => ("modified_date DESC, path", "modified_date"),
        SortKey::Name => ("filename, path", "filename"),
    };
    // Left to itself, SQLite reads every path's rows in sort order through that column's index,
    // filtering as it goes: quick when matches are plentiful, but a scan of every row when they
    // are few (minutes on tens of millions of rows). Then it's faster to find the matches
    // through the filter's own index and sort just them ("+" stops the sort index) — so count
    // the matches (up to the cap) and choose.
    let filtered_by: Vec<&str> = [
        (filters.larger_than.is_some() || filters.smaller_than.is_some(), "size"),
        (filters.older_than.is_some() || filters.newer_than.is_some(), "modified_date"),
        (filters.not_accessed.is_some(), "access_date"),
        (filters.ext.is_some(), "extension"),
        (filters.duplicate, "checksum"),
    ]
    .into_iter()
    .filter_map(|(on, col)| on.then_some(col))
    .collect();
    let count_sql = format!("{with} SELECT COUNT(*) FROM (SELECT 1 FROM files{where_sql} LIMIT {COUNT_CAP})");
    let few = || -> rusqlite::Result<bool> {
        let n: i64 = conn.query_row(&count_sql, rusqlite::params_from_iter(params.iter()), |r| r.get(0))?;
        Ok((n as usize) < COUNT_CAP)
    };
    let order = if filtered_by.is_empty() || filtered_by.contains(&column) {
        order.to_string()
    } else if few()? {
        format!("+{}", order) // find the few matches through the filter's index, then sort them
    } else {
        // Plenty of matches: read in sort order and stop at the limit
        if let Some(c) = parts_for(conn, scope, filters, true)?.conds {
            where_sql = format!(" WHERE {}", c);
        }
        order.to_string()
    };
    let limit_sql = limit.map_or(String::new(), |n| format!(" LIMIT {}", n));
    Ok(Query {
        sql: format!(
            "{with} SELECT root, path, filename, directory, extension, size, modified_date, access_date, \
             birth_date, checksum, device, inode FROM files{where_sql} ORDER BY {order}{limit_sql}"
        ),
        count_sql,
        params,
    })
}

pub(crate) fn build_parts(conn: &Connection, scope: &Scope, filters: &FilterArgs) -> rusqlite::Result<Parts> {
    parts_for(conn, scope, filters, false)
}

/// `walk_order`: keep the filters off their indexes ("+column"), so SQLite reads in the sort
/// order and stops at the limit — the fast way when matches are plentiful
fn parts_for(conn: &Connection, scope: &Scope, filters: &FilterArgs, walk_order: bool) -> rusqlite::Result<Parts> {
    let col = |c: &str| if walk_order { format!("+{c}") } else { c.to_string() };
    // Duplicates across every path come from the summary, which holds each group's size — so a
    // size filter goes there, and the files are found from the (few) matching groups
    let dup_summary = filters.duplicate && scope.everything && has_dup_groups(conn)?;
    let lowercase_ext = schema_version(conn)? < 1;
    let sources: Vec<String> = scope
        .tables
        .iter()
        .map(|t| source_select(conn, scope, t, lowercase_ext))
        .collect::<rusqlite::Result<_>>()?;
    let with = format!("WITH files AS NOT MATERIALIZED ({})", sources.join(" UNION ALL "));

    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(name) = &filters.name {
        if filters.ignore_case {
            conds.push("lower(filename) GLOB lower(?)".into());
        } else {
            conds.push("filename GLOB ?".into());
        }
        params.push(Value::Text(name.clone()));
    }
    if let Some(ExtList(exts)) = &filters.ext {
        conds.push(format!("{} IN ({})", col("extension"), vec!["?"; exts.len()].join(", ")));
        params.extend(exts.iter().map(|e| Value::Text(e.clone())));
    }
    if let Some(n) = filters.larger_than.filter(|_| !dup_summary) {
        conds.push(format!("{} > ?", col("size")));
        params.push(Value::Integer(n as i64));
    }
    if let Some(n) = filters.smaller_than.filter(|_| !dup_summary) {
        conds.push(format!("{} < ?", col("size")));
        params.push(Value::Integer(n as i64));
    }
    // Unknown dates are stored as '' — never treat them as "old"
    if let Some(cutoff) = &filters.older_than {
        conds.push(format!("modified_date != '' AND {} < ?", col("modified_date")));
        params.push(Value::Text(cutoff.clone()));
    }
    if let Some(cutoff) = &filters.newer_than {
        conds.push(format!("{} >= ?", col("modified_date")));
        params.push(Value::Text(cutoff.clone()));
    }
    if let Some(cutoff) = &filters.not_accessed {
        conds.push(format!("access_date != '' AND {} < ?", col("access_date")));
        params.push(Value::Text(cutoff.clone()));
    }
    for under in [filters.under.as_deref().map(absolute), scope.under.clone()].into_iter().flatten() {
        let prefix = if under == "/" { "/".to_string() } else { format!("{}/", under) };
        conds.push("(path = ? OR substr(path, 1, ?) = ?)".into());
        params.push(Value::Text(under.clone()));
        params.push(Value::Integer(prefix.chars().count() as i64));
        params.push(Value::Text(prefix));
    }
    if dup_summary {
        // Across every path: the duplicate summary already knows. Copies share a size, so a
        // size filter narrows the groups before any file is looked at.
        let mut within = Vec::new();
        if let Some(n) = filters.larger_than {
            within.push("size > ?");
            params.push(Value::Integer(n as i64));
        }
        if let Some(n) = filters.smaller_than {
            within.push("size < ?");
            params.push(Value::Integer(n as i64));
        }
        let where_groups = if within.is_empty() { String::new() } else { format!(" WHERE {}", within.join(" AND ")) };
        // SQLite can't tell how many groups match, and may check every file against them. When
        // they're few, list them: then each path's checksum index finds their files directly.
        let group_params: Vec<Value> = params.split_off(params.len() - within.len());
        let mut stmt = conn.prepare(&format!("SELECT checksum FROM dup_groups{} LIMIT {}", where_groups, LISTED_GROUPS + 1))?;
        let few: Vec<String> = stmt.query_map(params_from_iter(group_params.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        if few.len() <= LISTED_GROUPS {
            if few.is_empty() {
                conds.push("0".into()); // no group matches
            } else {
                conds.push(format!("{} IN ({})", col("checksum"), vec!["?"; few.len()].join(", ")));
                params.extend(few.into_iter().map(Value::Text));
            }
        } else {
            conds.push(format!("{} IN (SELECT checksum FROM dup_groups{})", col("checksum"), where_groups));
            params.extend(group_params);
        }
    } else if filters.duplicate {
        // Same rule as everywhere else: hard links (same device + inode) are one copy
        conds.push(
            "checksum IN (SELECT checksum FROM files \
             WHERE size > 0 AND checksum IS NOT NULL AND checksum != '' GROUP BY checksum \
             HAVING COUNT(DISTINCT copy_id) > 1)"
                .into(),
        );
    }

    let conds = (!conds.is_empty()).then(|| conds.join(" AND "));
    Ok(Parts { with, conds, params })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("200K"), Ok(200 * 1024));
        assert_eq!(parse_size("500MB"), Ok(500 * 1024 * 1024));
        assert_eq!(parse_size("1.5gb"), Ok(1610612736));
        assert_eq!(parse_size("2 TB"), Ok(2 << 40));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5XB").is_err());
    }

    #[test]
    fn ages() {
        let now = Utc::now();
        let ninety = parse_age("90d").unwrap();
        let expected = (now - chrono::Duration::days(90)).format("%Y-%m-%d").to_string();
        assert!(ninety.starts_with(&expected), "{} vs {}", ninety, expected);
        assert!(parse_age("1y").unwrap() < parse_age("6w").unwrap());
        assert_eq!(parse_age("2024-01-01").unwrap().len(), "2024-01-01 00:00:00".len());
        assert!(parse_age("90").is_err());
        assert!(parse_age("3x").is_err());
    }

    #[test]
    fn extension_lists() {
        assert_eq!(parse_ext_list("JPG, .mov,jpg").unwrap().0, vec!["jpg", "mov"]);
        assert_eq!(parse_ext_list("none,txt").unwrap().0, vec!["", "txt"]);
        assert!(parse_ext_list(" , ").is_err());
    }
}
