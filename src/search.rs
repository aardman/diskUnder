//! `search` and `export`: read-only queries over the index (never logged, never migrate)

use rusqlite::{params_from_iter, Connection};
use std::{io::IsTerminal, path::Path};

use crate::fmt::fmt_count;
use crate::output::{write_header, write_path, write_record, write_table, FileRow, Format, Out};
use crate::query::{build_query, resolve_scope, FilterArgs, Query, Scope, SortKey};

pub(crate) struct SearchOptions<'a> {
    pub(crate) path: Option<&'a str>,
    pub(crate) filters: &'a FilterArgs,
    pub(crate) sort: SortKey,
    pub(crate) limit: usize, // 0 = no limit
    pub(crate) format: Format,
    pub(crate) paths: bool,
    pub(crate) null: bool,
    pub(crate) long: bool,
}

/// Open the database and work out what to read, printing freshness notes to stderr
fn prepare(db_path: &str, path: Option<&str>) -> Result<(Connection, Scope), String> {
    // A query shouldn't create a database
    if !Path::new(db_path).exists() {
        return Err(format!("no database at {}", db_path));
    }
    let conn = Connection::open(db_path).map_err(|e| format!("{}: {}", db_path, e))?;
    let scope = resolve_scope(&conn, path)?;
    for note in scope.freshness_notes() {
        eprintln!("{}", note);
    }
    Ok((conn, scope))
}

fn for_each_row(conn: &Connection, q: &Query, mut f: impl FnMut(FileRow)) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&q.sql)?;
    let mut rows = stmt.query(params_from_iter(q.params.iter()))?;
    while let Some(row) = rows.next()? {
        f(FileRow::from_row(row)?);
    }
    Ok(())
}

fn search(db_path: &str, o: &SearchOptions) -> Result<(), String> {
    let (conn, scope) = prepare(db_path, o.path)?;
    let db = |e: rusqlite::Error| e.to_string();

    // Fetch one extra row to know whether the limit cut anything off
    let fetch = (o.limit > 0).then_some(o.limit + 1);
    let q = build_query(&conn, &scope, o.filters, o.sort, fetch).map_err(db)?;
    let mut rows = Vec::new();
    for_each_row(&conn, &q, |r| rows.push(r)).map_err(db)?;

    let more = if o.limit > 0 && rows.len() > o.limit {
        rows.truncate(o.limit);
        let total: i64 = conn
            .query_row(&q.count_sql, params_from_iter(q.params.iter()), |r| r.get(0))
            .map_err(db)?;
        Some(total as usize - o.limit)
    } else {
        None
    };

    let mut out = Out::new();
    if o.paths || o.null {
        for r in &rows {
            write_path(&mut out, &r.path, o.null);
        }
    } else if o.format == Format::Table {
        if !rows.is_empty() {
            let width = std::io::stdout()
                .is_terminal()
                .then(|| crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(120));
            write_table(&mut out, &rows, o.long, width);
        }
    } else {
        write_header(&mut out, o.format);
        for r in &rows {
            write_record(&mut out, o.format, r);
        }
    }
    out.flush();

    if rows.is_empty() {
        eprintln!("no matching files");
    }
    if let Some(n) = more {
        eprintln!("… {} more — use --limit to show more, or --limit 0 for all", fmt_count(n as i64));
    }
    Ok(())
}

fn export(db_path: &str, path: Option<&str>, filters: &FilterArgs, sort: SortKey, format: Format) -> Result<(), String> {
    let (conn, scope) = prepare(db_path, path)?;
    let db = |e: rusqlite::Error| e.to_string();
    let q = build_query(&conn, &scope, filters, sort, None).map_err(db)?;

    // Streamed row by row, so a million-row export never sits in memory
    let mut out = Out::new();
    write_header(&mut out, format);
    let mut n: i64 = 0;
    for_each_row(&conn, &q, |r| {
        write_record(&mut out, format, &r);
        n += 1;
    })
    .map_err(db)?;
    out.flush();
    eprintln!("exported {} files", fmt_count(n));
    Ok(())
}

fn exit_code(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

pub(crate) fn run_search(db_path: &str, o: &SearchOptions) -> i32 {
    exit_code(search(db_path, o))
}

pub(crate) fn run_export(db_path: &str, path: Option<&str>, filters: &FilterArgs, sort: SortKey, format: Format) -> i32 {
    exit_code(export(db_path, path, filters, sort, format))
}
