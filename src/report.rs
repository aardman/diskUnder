//! `report`: summaries of an index — largest files, space by type and by folder, stale files
//! and duplicate groups. Read-only like `search`: never logged, never migrates.

use rusqlite::{params_from_iter, types::Value, Connection};
use std::{
    collections::{HashMap, HashSet},
    io::IsTerminal,
    path::{Path, PathBuf},
};

use crate::fmt::{fmt_bytes, fmt_count, fmt_local, truncate_path};
use crate::output::{
    write_cells, write_header, write_record, write_table, write_text_table, Align, Cell, FileRow, Format, Out,
};
use crate::query::{absolute, build_parts, build_query, parse_age, resolve_scope, FilterArgs, Scope, SortKey};

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub(crate) enum StaleBy {
    /// Not modified within AGE (reliable)
    Modified,
    /// Not accessed within AGE (unreliable on many file servers)
    Access,
}

/// An age as typed (for headings) and the cutoff it means
#[derive(Clone)]
pub(crate) struct Age {
    pub(crate) input: String,
    pub(crate) cutoff: String,
}

pub(crate) fn parse_age_arg(s: &str) -> Result<Age, String> {
    Ok(Age { input: s.trim().to_string(), cutoff: parse_age(s)? })
}

pub(crate) struct ReportOptions<'a> {
    pub(crate) path: Option<&'a str>,
    pub(crate) filters: &'a FilterArgs,
    pub(crate) top_files: Option<Option<usize>>, // Some(None) = flag given without a count
    pub(crate) by_type: bool,
    pub(crate) by_directory: bool,
    pub(crate) depth: usize,
    pub(crate) stale: Option<&'a Age>,
    pub(crate) stale_by: StaleBy,
    pub(crate) duplicates: bool,
    pub(crate) min_size: Option<u64>,
    pub(crate) min_copies: usize,
    pub(crate) min_wasted: Option<u64>,
    pub(crate) all_paths: bool,
    pub(crate) limit: Option<usize>, // None = the section's default; Some(0) = no limit
    pub(crate) format: Format,
}

const SECTION_ROWS: usize = 20;
const OVERVIEW_ROWS: usize = 10;
const OVERVIEW_GROUPS: usize = 5;
const PATHS_PER_GROUP: usize = 5;
const OVERVIEW_PATHS_PER_GROUP: usize = 3;
const OVERVIEW_STALE: &str = "1y";

type Group = (String, i64, i64, i64, String); // checksum, size, copies, wasted, first path

struct Report<'a> {
    groups: Option<Vec<Group>>, // computed once, shared by the overview and the section
    conn: Connection,
    scope: Scope,
    out: Out,
    format: Format,
    width: Option<usize>, // terminal width when stdout is a terminal; paths are shortened to fit
    o: &'a ReportOptions<'a>,
}

/// Rows to show: the user's --limit (0 = all), else the section's default
fn rows(limit: Option<usize>, default: usize) -> Option<usize> {
    match limit.unwrap_or(default) {
        0 => None,
        n => Some(n),
    }
}

/// "1 group", "3 groups"
fn plural(n: i64, word: &str) -> String {
    format!("{} {}{}", fmt_count(n), word, if n == 1 { "" } else { "s" })
}

fn share(part: i64, total: i64) -> f64 {
    if total > 0 { part as f64 * 100.0 / total as f64 } else { 0.0 }
}

impl Report<'_> {
    fn db(e: rusqlite::Error) -> String {
        e.to_string()
    }

    /// Long paths shortened to fit the terminal, leaving `used` columns for the rest
    fn fit(&self, path: &str, used: usize) -> String {
        match self.width {
            Some(w) => truncate_path(path, w.saturating_sub(used).max(30)),
            None => path.to_string(),
        }
    }

    fn heading(&mut self, title: &str) {
        self.out.write(&format!("{}\n", title));
    }

    fn totals(&self, filters: &FilterArgs) -> Result<(i64, i64), String> {
        let p = build_parts(&self.conn, &self.scope, filters).map_err(Self::db)?;
        self.conn
            .query_row(
                &format!("{} SELECT COUNT(*), COALESCE(SUM(size), 0) FROM files{}", p.with, p.where_sql()),
                params_from_iter(p.params.iter()),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(Self::db)
    }

    // ── Largest files ──

    fn top_files(&mut self, title: &str, filters: &FilterArgs, n: Option<usize>) -> Result<(), String> {
        let q = build_query(&self.conn, &self.scope, filters, SortKey::Size, n).map_err(Self::db)?;
        let found: Vec<FileRow> = {
            let mut stmt = self.conn.prepare(&q.sql).map_err(Self::db)?;
            let rows = stmt
                .query_map(params_from_iter(q.params.iter()), FileRow::from_row)
                .map_err(Self::db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(Self::db)?;
            rows
        };
        if self.format == Format::Table {
            self.heading(title);
            if found.is_empty() {
                self.out.write("  (none)\n");
            } else {
                write_table(&mut self.out, &found, false, self.width);
            }
        } else {
            write_header(&mut self.out, self.format);
            for r in &found {
                write_record(&mut self.out, self.format, r);
            }
        }
        Ok(())
    }

    // ── Space by type ──

    fn by_type(&mut self, filters: &FilterArgs, n: Option<usize>) -> Result<(), String> {
        let p = build_parts(&self.conn, &self.scope, filters).map_err(Self::db)?;
        let limit = n.map_or(String::new(), |n| format!(" LIMIT {}", n));
        // The window total covers every type, not just the ones LIMIT keeps
        let sql = format!(
            "{} SELECT extension, COUNT(*), COALESCE(SUM(size), 0) AS bytes, SUM(SUM(size)) OVER () \
             FROM files{} GROUP BY extension ORDER BY bytes DESC, extension{}",
            p.with,
            p.where_sql(),
            limit
        );
        let mut total = 0i64;
        let found: Vec<(String, i64, i64)> = {
            let mut stmt = self.conn.prepare(&sql).map_err(Self::db)?;
            let rows = stmt
                .query_map(params_from_iter(p.params.iter()), |r| {
                    total = r.get::<_, Option<i64>>(3)?.unwrap_or(0);
                    Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?))
                })
                .map_err(Self::db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(Self::db)?;
            rows
        };

        if self.format == Format::Table {
            self.heading("Space by type");
            let table: Vec<Vec<String>> = found
                .iter()
                .map(|(ext, files, bytes)| {
                    vec![
                        if ext.is_empty() { "(none)".to_string() } else { ext.clone() },
                        fmt_count(*files),
                        fmt_bytes(*bytes as u64),
                        format!("{:.1}%", share(*bytes, total)),
                    ]
                })
                .collect();
            write_text_table(
                &mut self.out,
                &["TYPE", "FILES", "SIZE", "SHARE"],
                &[Align::Left, Align::Right, Align::Right, Align::Right],
                &table,
            );
        } else {
            let cells: Vec<Vec<Cell>> = found
                .into_iter()
                .map(|(ext, files, bytes)| {
                    vec![Cell::Text(ext), Cell::Int(files), Cell::Int(bytes), Cell::Dec(share(bytes, total))]
                })
                .collect();
            write_cells(&mut self.out, self.format, &["type", "files", "size", "share_percent"], &cells);
        }
        Ok(())
    }

    // ── Space by folder ──

    /// The folder a file's directory rolls up to: `base` plus at most `depth` more levels
    fn roll_up(base: &str, directory: &str, depth: usize) -> String {
        match Path::new(directory).strip_prefix(base) {
            Ok(rel) => {
                let mut key = PathBuf::from(base);
                key.extend(rel.components().take(depth));
                key.to_string_lossy().into_owned()
            }
            Err(_) => directory.to_string(),
        }
    }

    fn by_directory(&mut self, title: &str, filters: &FilterArgs, n: Option<usize>) -> Result<(), String> {
        let p = build_parts(&self.conn, &self.scope, filters).map_err(Self::db)?;
        let sql = format!(
            "{} SELECT root, directory, COUNT(*), COALESCE(SUM(size), 0) FROM files{} GROUP BY root, directory",
            p.with,
            p.where_sql()
        );
        let explicit_under = filters.under.as_deref().map(absolute);
        let mut folders: HashMap<String, (i64, i64)> = HashMap::new();
        let mut total = 0i64;
        let mut stmt = self.conn.prepare(&sql).map_err(Self::db)?;
        let mut rows = stmt.query(params_from_iter(p.params.iter())).map_err(Self::db)?;
        while let Some(r) = rows.next().map_err(Self::db)? {
            let root: String = r.get(0).map_err(Self::db)?;
            let directory: String = r.get::<_, Option<String>>(1).map_err(Self::db)?.unwrap_or_default();
            let (files, bytes): (i64, i64) = (r.get(2).map_err(Self::db)?, r.get(3).map_err(Self::db)?);
            let base = explicit_under.clone().or_else(|| self.scope.under.clone()).unwrap_or(root);
            let entry = folders.entry(Self::roll_up(&base, &directory, self.o.depth)).or_default();
            entry.0 += files;
            entry.1 += bytes;
            total += bytes;
        }
        drop(rows);
        drop(stmt);

        let mut sorted: Vec<(String, i64, i64)> = folders.into_iter().map(|(k, (f, b))| (k, f, b)).collect();
        sorted.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        if let Some(n) = n {
            sorted.truncate(n);
        }

        if self.format == Format::Table {
            self.heading(title);
            let table: Vec<Vec<String>> = sorted
                .iter()
                .map(|(folder, files, bytes)| {
                    vec![
                        fmt_count(*files),
                        fmt_bytes(*bytes as u64),
                        format!("{:.1}%", share(*bytes, total)),
                        self.fit(folder, 34),
                    ]
                })
                .collect();
            write_text_table(
                &mut self.out,
                &["FILES", "SIZE", "SHARE", "FOLDER"],
                &[Align::Right, Align::Right, Align::Right, Align::Left],
                &table,
            );
        } else {
            let cells: Vec<Vec<Cell>> = sorted
                .into_iter()
                .map(|(folder, files, bytes)| {
                    vec![Cell::Text(folder), Cell::Int(files), Cell::Int(bytes), Cell::Dec(share(bytes, total))]
                })
                .collect();
            write_cells(&mut self.out, self.format, &["folder", "files", "size", "share_percent"], &cells);
        }
        Ok(())
    }

    // ── Stale files ──

    /// The user's filters plus the staleness condition (the stricter cutoff wins)
    fn stale_filters(&self, age: &Age, by: StaleBy) -> FilterArgs {
        let mut f = self.o.filters.clone();
        let slot = match by {
            StaleBy::Modified => &mut f.older_than,
            StaleBy::Access => &mut f.not_accessed,
        };
        *slot = Some(match slot.take() {
            Some(existing) if existing < age.cutoff => existing,
            _ => age.cutoff.clone(),
        });
        f
    }

    fn stale_label(age: &Age, by: StaleBy) -> String {
        match by {
            StaleBy::Modified => format!("Not modified in {}", age.input),
            StaleBy::Access => format!("Not accessed in {}", age.input),
        }
    }

    fn stale(&mut self, age: &Age, n: Option<usize>) -> Result<(), String> {
        let by = self.o.stale_by;
        let stale = self.stale_filters(age, by);
        if self.format != Format::Table {
            return self.top_files("", &stale, n);
        }
        let (all_files, all_bytes) = self.totals(self.o.filters)?;
        let (files, bytes) = self.totals(&stale)?;
        let label = Self::stale_label(age, by);
        self.heading(&format!(
            "{}: {} of {} files · {} ({:.1}% of the space)",
            label,
            fmt_count(files),
            fmt_count(all_files),
            fmt_bytes(bytes as u64),
            share(bytes, all_bytes)
        ));
        if by == StaleBy::Access {
            self.out.write("  (access times are unreliable on many file servers — see --stale-by)\n");
        }
        self.out.write("\n");
        let depth = self.o.depth;
        self.by_directory(&format!("{} — by folder (depth {})", label, depth), &stale, n)?;
        self.out.write("\n");
        self.top_files(&format!("{} — largest files", label), &stale, n)
    }

    // ── Duplicates ──

    /// Every duplicate group matching the filters, most wasted space first:
    /// (checksum, size, copies, wasted, first path)
    fn duplicate_groups(&mut self) -> Result<Vec<Group>, String> {
        if let Some(g) = &self.groups {
            return Ok(g.clone());
        }
        let groups = self.query_duplicate_groups()?;
        self.groups = Some(groups.clone());
        Ok(groups)
    }

    fn query_duplicate_groups(&self) -> Result<Vec<Group>, String> {
        let p = build_parts(&self.conn, &self.scope, self.o.filters).map_err(Self::db)?;
        // Shared filters pick groups with at least one matching copy; groups always list every copy
        let mut conds = vec![format!("copies >= {}", self.o.min_copies.max(2))];
        let mut params: Vec<Value> = Vec::new();
        if let Some(c) = &p.conds {
            conds.insert(0, format!("checksum IN (SELECT checksum FROM files WHERE {})", c));
            params.extend(p.params.iter().cloned());
        }
        if let Some(n) = self.o.min_size {
            conds.push("size >= ?".into());
            params.push(Value::Integer(n as i64));
        }
        if let Some(n) = self.o.min_wasted {
            conds.push("size * (copies - 1) >= ?".into());
            params.push(Value::Integer(n as i64));
        }
        let sql = format!(
            "{}, groups AS (
                 SELECT checksum, MAX(size) AS size, MIN(path) AS first_path,
                        COUNT(DISTINCT CASE WHEN inode IS NOT NULL THEN device || ':' || inode END)
                          + SUM(inode IS NULL) AS copies
                 FROM files
                 WHERE size > 0 AND checksum IS NOT NULL AND checksum != ''
                 GROUP BY checksum)
             SELECT checksum, size, copies, size * (copies - 1) AS wasted, first_path
             FROM groups WHERE {} ORDER BY wasted DESC, checksum",
            p.with,
            conds.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql).map_err(Self::db)?;
        let groups = stmt
            .query_map(params_from_iter(params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(Self::db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(Self::db)?;
        Ok(groups)
    }

    /// Every copy of the given groups: checksum → [(path, modified, device, inode)], by path
    fn group_members(
        &self,
        checksums: &[&str],
    ) -> Result<HashMap<String, Vec<(String, String, Option<i64>, Option<i64>)>>, String> {
        let with = build_parts(&self.conn, &self.scope, &FilterArgs::default()).map_err(Self::db)?.with;
        let mut members: HashMap<String, Vec<_>> = HashMap::new();
        // In chunks, so --limit 0 on a share with many groups stays under SQLite's variable limit
        for chunk in checksums.chunks(500) {
            let sql = format!(
                "{} SELECT checksum, path, modified_date, device, inode FROM files \
                 WHERE checksum IN ({}) ORDER BY checksum, path",
                with,
                vec!["?"; chunk.len()].join(", ")
            );
            let mut stmt = self.conn.prepare(&sql).map_err(Self::db)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter()), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        (
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            r.get::<_, Option<i64>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                        ),
                    ))
                })
                .map_err(Self::db)?;
            for row in rows {
                let (checksum, member) = row.map_err(Self::db)?;
                members.entry(checksum).or_default().push(member);
            }
        }
        Ok(members)
    }

    fn name_of(path: &str) -> String {
        Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
    }

    fn duplicates(&mut self, n: Option<usize>, paths_per_group: Option<usize>, summary: bool) -> Result<(), String> {
        let groups = self.duplicate_groups()?;
        let shown = &groups[..n.map_or(groups.len(), |n| n.min(groups.len()))];
        let checksums: Vec<&str> = shown.iter().map(|g| g.0.as_str()).collect();
        let members = self.group_members(&checksums)?;

        if self.format != Format::Table {
            let mut cells = Vec::new();
            for (i, (checksum, size, copies, wasted, _)) in shown.iter().enumerate() {
                for (path, modified, device, inode) in members.get(checksum).into_iter().flatten() {
                    let iso = if modified.is_empty() { String::new() } else { format!("{}Z", modified.replacen(' ', "T", 1)) };
                    cells.push(vec![
                        Cell::Int(i as i64 + 1),
                        Cell::Text(checksum.clone()),
                        Cell::Int(*size),
                        Cell::Int(*copies),
                        Cell::Int(*wasted),
                        Cell::Text(path.clone()),
                        Cell::Text(iso),
                        device.map_or(Cell::Null, Cell::Int),
                        inode.map_or(Cell::Null, Cell::Int),
                    ]);
                }
            }
            write_cells(
                &mut self.out,
                self.format,
                &["group", "checksum", "size", "copies", "wasted", "path", "modified", "device", "inode"],
                &cells,
            );
            return Ok(());
        }

        let title = if shown.len() < groups.len() {
            format!("Duplicates — largest {} of {} groups", shown.len(), fmt_count(groups.len() as i64))
        } else {
            "Duplicates".to_string()
        };
        self.heading(&title);
        if groups.is_empty() {
            self.out.write("  (none)\n");
        }
        for (checksum, size, copies, wasted, first) in shown {
            self.out.write(&format!(
                "  {} wasted   {} — {} × {} copies\n",
                fmt_bytes(*wasted as u64),
                Self::name_of(first),
                fmt_bytes(*size as u64),
                copies
            ));
            let list = members.get(checksum).map(Vec::as_slice).unwrap_or(&[]);
            let mut seen: HashSet<(i64, i64)> = HashSet::new();
            let cap = paths_per_group.unwrap_or(list.len());
            for (path, modified, device, inode) in list.iter().take(cap) {
                let link = match (device, inode) {
                    (Some(d), Some(i)) if !seen.insert((*d, *i)) => "  (hard link)",
                    _ => "",
                };
                let when = if modified.is_empty() { "-".to_string() } else { fmt_local(modified) };
                let line = format!("    {:<16}  {}{}\n", when, self.fit(path, 22 + link.len()), link);
                self.out.write(&line);
            }
            if list.len() > cap {
                self.out.write(&format!("    … and {} more\n", list.len() - cap));
            }
        }
        if summary {
            let copies: i64 = groups.iter().map(|g| g.2 - 1).sum();
            let wasted: i64 = groups.iter().map(|g| g.3).sum();
            self.out.write("\n");
            self.out.write(&format!("  Duplicate groups       : {}\n", fmt_count(groups.len() as i64)));
            self.out.write(&format!("  Files that are copies  : {}\n", fmt_count(copies)));
            self.out.write(&format!("  Total wasted space     : {}\n", fmt_bytes(wasted as u64)));
            if let Some((_, size, copies, wasted, first)) = groups.first() {
                self.out.write(&format!(
                    "  Largest group          : {} — {} × {} copies ({} wasted)\n",
                    Self::name_of(first),
                    fmt_bytes(*size as u64),
                    copies,
                    fmt_bytes(*wasted as u64)
                ));
            }
        }
        Ok(())
    }

    // ── Overview ──

    fn overview(&mut self) -> Result<(), String> {
        let (files, bytes) = self.totals(self.o.filters)?;
        let groups = self.duplicate_groups()?;
        let wasted: i64 = groups.iter().map(|g| g.3).sum();
        let age = parse_age_arg(OVERVIEW_STALE)?;
        let stale = self.stale_filters(&age, StaleBy::Modified);
        let (stale_files, stale_bytes) = self.totals(&stale)?;

        let label = Self::stale_label(&age, StaleBy::Modified);
        let summary = vec![
            vec!["Files".to_string(), format!("{} · {}", fmt_count(files), fmt_bytes(bytes as u64))],
            vec![
                "Duplicates".to_string(),
                format!("{} · {} wasted", plural(groups.len() as i64, "group"), fmt_bytes(wasted as u64)),
            ],
            vec![
                label,
                format!(
                    "{} · {} ({:.1}% of the space)",
                    plural(stale_files, "file"),
                    fmt_bytes(stale_bytes as u64),
                    share(stale_bytes, bytes)
                ),
            ],
        ];
        for row in summary {
            self.out.write(&format!("  {:<22} {}\n", row[0], row[1]));
        }

        let n = rows(self.o.limit, OVERVIEW_ROWS);
        let filters = self.o.filters.clone();
        self.out.write("\n");
        self.top_files("Largest files", &filters, n)?;
        self.out.write("\n");
        self.by_type(&filters, n)?;
        self.out.write("\n");
        self.by_directory(&format!("Space by folder (depth {})", self.o.depth), &filters, n)?;
        self.out.write("\n");
        self.duplicates(rows(self.o.limit, OVERVIEW_GROUPS), Some(OVERVIEW_PATHS_PER_GROUP), false)
    }

    fn run(&mut self) -> Result<(), String> {
        let o = self.o;
        let sections = [o.top_files.is_some(), o.by_type, o.by_directory, o.stale.is_some(), o.duplicates]
            .iter()
            .filter(|s| **s)
            .count();
        if sections == 0 {
            if self.format != Format::Table {
                return Err("choose one section (e.g. --by-type) for --format csv, tsv or json".to_string());
            }
            return self.overview();
        }
        if sections > 1 && self.format != Format::Table {
            return Err("--format csv, tsv or json works with one section at a time".to_string());
        }

        let n = rows(o.limit, SECTION_ROWS);
        let filters = o.filters.clone();
        let mut first = true;
        let mut gap = |out: &mut Out| {
            if !std::mem::take(&mut first) {
                out.write("\n");
            }
        };
        if let Some(count) = o.top_files {
            gap(&mut self.out);
            // `--top-files N` sets this section's length; otherwise --limit or the default
            let top_n = count.map_or(n, |c| (c > 0).then_some(c));
            self.top_files("Largest files", &filters, top_n)?;
        }
        if o.by_type {
            gap(&mut self.out);
            self.by_type(&filters, n)?;
        }
        if o.by_directory {
            gap(&mut self.out);
            self.by_directory(&format!("Space by folder (depth {})", o.depth), &filters, n)?;
        }
        if let Some(age) = o.stale {
            gap(&mut self.out);
            self.stale(age, n)?;
        }
        if o.duplicates {
            gap(&mut self.out);
            let per_group = (!o.all_paths).then_some(PATHS_PER_GROUP);
            self.duplicates(n, per_group, true)?;
        }
        Ok(())
    }
}

fn report(db_path: &str, o: &ReportOptions) -> Result<(), String> {
    // A report shouldn't create a database
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    let scope = resolve_scope(&conn, o.path)?;
    for note in scope.freshness_notes() {
        eprintln!("{}", note);
    }
    let width = std::io::stdout()
        .is_terminal()
        .then(|| crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(120));
    let mut r = Report { groups: None, conn, scope, out: Out::new(), format: o.format, width, o };
    let result = r.run();
    r.out.flush();
    result
}

pub(crate) fn run_report(db_path: &str, o: &ReportOptions) -> i32 {
    match report(db_path, o) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}
