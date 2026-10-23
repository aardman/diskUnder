//! Writing query results to stdout: a readable table, CSV, TSV, newline-delimited JSON, or
//! bare paths. Only data goes to stdout; notes and warnings go to stderr.

use std::io::{self, BufWriter, ErrorKind, Stdout, Write};

use crate::fmt::{fmt_bytes, fmt_local, truncate_path};
use crate::query::COLUMNS;

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub(crate) enum Format {
    /// Readable columns (the same whether or not output is piped)
    Table,
    /// Comma-separated, with a header row
    Csv,
    /// Tab-separated, with a header row
    Tsv,
    /// One JSON object per line
    Json,
}

pub(crate) struct FileRow {
    pub(crate) root: String,
    pub(crate) path: String,
    pub(crate) filename: String,
    pub(crate) directory: String,
    pub(crate) extension: String,
    pub(crate) size: i64,
    pub(crate) modified: String, // stored UTC, "YYYY-MM-DD HH:MM:SS"; "" if unknown
    pub(crate) accessed: String,
    pub(crate) created: String,
    pub(crate) checksum: String,
    pub(crate) device: Option<i64>,
    pub(crate) inode: Option<i64>,
}

impl FileRow {
    /// Reads a row in the column order of `query::COLUMNS`
    pub(crate) fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let text = |i: usize| -> rusqlite::Result<String> { Ok(row.get::<_, Option<String>>(i)?.unwrap_or_default()) };
        Ok(FileRow {
            root: text(0)?,
            path: text(1)?,
            filename: text(2)?,
            directory: text(3)?,
            extension: text(4)?,
            size: row.get(5)?,
            modified: text(6)?,
            accessed: text(7)?,
            created: text(8)?,
            checksum: text(9)?,
            device: row.get(10)?,
            inode: row.get(11)?,
        })
    }
}

/// Buffered stdout that exits quietly when the reader goes away (e.g. `| head`) — or a
/// text buffer, for output the interactive app shows itself
pub(crate) enum Out {
    Stdout(BufWriter<Stdout>),
    Buffer(String),
}

impl Out {
    pub(crate) fn new() -> Self {
        Out::Stdout(BufWriter::new(io::stdout()))
    }

    pub(crate) fn buffer() -> Self {
        Out::Buffer(String::new())
    }

    /// What was written to a buffer (empty for stdout)
    pub(crate) fn into_text(self) -> String {
        match self {
            Out::Buffer(s) => s,
            Out::Stdout(_) => String::new(),
        }
    }

    pub(crate) fn write(&mut self, s: &str) {
        match self {
            Out::Stdout(w) => {
                if let Err(e) = w.write_all(s.as_bytes()) {
                    Self::fail(e);
                }
            }
            Out::Buffer(b) => b.push_str(s),
        }
    }

    pub(crate) fn flush(&mut self) {
        if let Out::Stdout(w) = self {
            if let Err(e) = w.flush() {
                Self::fail(e);
            }
        }
    }

    fn fail(e: io::Error) -> ! {
        if e.kind() == ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("error: writing output: {}", e);
        std::process::exit(1);
    }
}

/// Stored UTC timestamp → ISO 8601 (2024-11-01T10:22:05Z) for machine formats
fn iso(utc: &str) -> String {
    if utc.is_empty() {
        String::new()
    } else {
        format!("{}Z", utc.replacen(' ', "T", 1))
    }
}

fn values(r: &FileRow) -> [String; 12] {
    let opt = |v: Option<i64>| v.map(|n| (n as u64).to_string()).unwrap_or_default();
    [
        r.root.clone(),
        r.path.clone(),
        r.filename.clone(),
        r.directory.clone(),
        r.extension.clone(),
        r.size.to_string(),
        iso(&r.modified),
        iso(&r.accessed),
        iso(&r.created),
        r.checksum.clone(),
        opt(r.device),
        opt(r.inode),
    ]
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn tsv_field(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Header row for CSV and TSV; nothing for the other formats
pub(crate) fn write_header(out: &mut Out, format: Format) {
    match format {
        Format::Csv => out.write(&format!("{}\n", COLUMNS.join(","))),
        Format::Tsv => out.write(&format!("{}\n", COLUMNS.join("\t"))),
        Format::Table | Format::Json => {}
    }
}

/// One row in a machine format (CSV, TSV or JSON)
pub(crate) fn write_record(out: &mut Out, format: Format, row: &FileRow) {
    let v = values(row);
    let line = match format {
        Format::Csv => v.iter().map(|s| csv_field(s)).collect::<Vec<_>>().join(","),
        Format::Tsv => v.iter().map(|s| tsv_field(s)).collect::<Vec<_>>().join("\t"),
        Format::Json => {
            let fields: Vec<String> = COLUMNS
                .iter()
                .zip(v.iter())
                .map(|(k, val)| {
                    let json_val = match *k {
                        "size" => val.clone(),
                        "device" | "inode" if val.is_empty() => "null".to_string(),
                        "device" | "inode" => val.clone(),
                        _ => json_string(val),
                    };
                    format!("{}:{}", json_string(k), json_val)
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Format::Table => unreachable!("tables are written with write_table"),
    };
    out.write(&line);
    out.write("\n");
}

pub(crate) fn write_path(out: &mut Out, path: &str, nul: bool) {
    out.write(path);
    out.write(if nul { "\0" } else { "\n" });
}

/// Readable columns. `width` is the terminal width when stdout is a terminal — long paths
/// are shortened to fit — and None when piped, so tools see full paths.
pub(crate) fn write_table(out: &mut Out, rows: &[FileRow], long: bool, width: Option<usize>) {
    let date = |utc: &str| if utc.is_empty() { "-".to_string() } else { fmt_local(utc) };
    let mut header = format!("{:>9}  {:<16}  ", "SIZE", "MODIFIED");
    if long {
        header.push_str(&format!("{:<16}  {:<16}  {:<12}  ", "ACCESSED", "CREATED", "CHECKSUM"));
    }
    let path_width = width.map(|w| w.saturating_sub(header.chars().count()).max(20));
    out.write(&format!("{}PATH\n", header));

    for r in rows {
        let mut line = format!("{:>9}  {:<16}  ", fmt_bytes(r.size as u64), date(&r.modified));
        if long {
            let short: String = r.checksum.chars().take(12).collect();
            line.push_str(&format!("{:<16}  {:<16}  {:<12}  ", date(&r.accessed), date(&r.created), short));
        }
        match path_width {
            Some(w) => line.push_str(&truncate_path(&r.path, w)),
            None => line.push_str(&r.path),
        }
        line.push('\n');
        out.write(&line);
    }
}

// ─── Generic rows (report sections that aren't plain file lists) ─────────────

/// A value in a report row. Numbers stay numbers in JSON; `Text` is quoted.
pub(crate) enum Cell {
    Text(String),
    Int(i64),
    Dec(f64), // one decimal place, e.g. a percentage
    Null,
}

impl Cell {
    fn plain(&self) -> String {
        match self {
            Cell::Text(s) => s.clone(),
            Cell::Int(n) => n.to_string(),
            Cell::Dec(x) => format!("{:.1}", x),
            Cell::Null => String::new(),
        }
    }

    fn json(&self) -> String {
        match self {
            Cell::Text(s) => json_string(s),
            Cell::Null => "null".to_string(),
            other => other.plain(),
        }
    }
}

/// Rows in a machine format, with `headers` as the CSV/TSV header row and JSON keys
pub(crate) fn write_cells(out: &mut Out, format: Format, headers: &[&str], rows: &[Vec<Cell>]) {
    match format {
        Format::Csv => out.write(&format!("{}\n", headers.join(","))),
        Format::Tsv => out.write(&format!("{}\n", headers.join("\t"))),
        Format::Json | Format::Table => {}
    }
    for row in rows {
        let line = match format {
            Format::Csv => row.iter().map(|c| csv_field(&c.plain())).collect::<Vec<_>>().join(","),
            Format::Tsv => row.iter().map(|c| tsv_field(&c.plain())).collect::<Vec<_>>().join("\t"),
            Format::Json => {
                let fields: Vec<String> =
                    headers.iter().zip(row).map(|(k, c)| format!("{}:{}", json_string(k), c.json())).collect();
                format!("{{{}}}", fields.join(","))
            }
            Format::Table => unreachable!("tables are written with write_text_table"),
        };
        out.write(&line);
        out.write("\n");
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Align {
    Left,
    Right,
}

/// An indented, aligned text table of already-formatted values. The last column is left
/// unpadded so long paths don't push lines wider than they need to be.
pub(crate) fn write_text_table(out: &mut Out, headers: &[&str], aligns: &[Align], rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).chain([headers[i].len()]).max().unwrap_or(0))
        .collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::from("  ");
        for (i, c) in cells.iter().enumerate() {
            let last = i + 1 == cells.len();
            let pad = widths[i].saturating_sub(c.chars().count());
            match aligns[i] {
                Align::Right => s.push_str(&format!("{}{}", " ".repeat(pad), c)),
                Align::Left if last => s.push_str(c),
                Align::Left => s.push_str(&format!("{}{}", c, " ".repeat(pad))),
            }
            if !last {
                s.push_str("  ");
            }
        }
        s.push('\n');
        s
    };
    out.write(&line(headers.to_vec()));
    for r in rows {
        out.write(&line(r.iter().map(String::as_str).collect()));
    }
}
