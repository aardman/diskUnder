//! Command-line arguments

use clap::{Parser, Subcommand};

use crate::output::Format;
use crate::query::{FilterArgs, SortKey};

#[derive(Parser)]
#[command(name = "diskunder", about = "File indexer with live TUI progress", version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Command,

    /// SQLite database file (created if absent)
    #[arg(long, default_value = "fileindex.db", global = true)]
    pub(crate) db: String,

    /// Parallel hashing threads — keep low for network shares (e.g. --threads 4)
    #[arg(long, default_value_t = 4, global = true)]
    pub(crate) threads: usize,

    /// Disable the TUI and print plain progress lines instead.
    /// Enabled automatically when stdout is not a terminal (cron, SSH, pipes).
    #[arg(long, global = true)]
    pub(crate) no_tui: bool,

    /// Run log file for commands that change the database (default: <db>.log)
    #[arg(long, global = true, value_name = "FILE")]
    pub(crate) log: Option<String>,

    /// Don't write the run log
    #[arg(long, global = true, conflicts_with = "log")]
    pub(crate) no_log: bool,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Index every file under PATH; files already in the database are skipped,
    /// so an interrupted run can simply be restarted
    Index {
        /// Directory to index
        path: String,
    },
    /// Re-index new and changed files under PATH, and drop rows for deleted files
    Update {
        /// Directory to update
        path: String,
    },
    /// Find indexed files by name, type, size, age or duplication
    Search {
        /// Indexed path, or a folder inside one (default: every indexed path)
        path: Option<String>,
        #[command(flatten)]
        filters: FilterArgs,
        /// Order of results
        #[arg(long, value_enum, default_value_t = SortKey::Path)]
        sort: SortKey,
        /// Maximum results to show; 0 for no limit
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Output format
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
        /// Print only paths, one per line
        #[arg(long)]
        paths: bool,
        /// Print only paths, separated by NUL (for xargs -0)
        #[arg(short = '0', long = "null")]
        null: bool,
        /// Also show access and creation dates and checksum (table format)
        #[arg(long)]
        long: bool,
    },
    /// Write indexed files to CSV, TSV or JSON — every column, no row limit
    Export {
        /// Indexed path, or a folder inside one (default: every indexed path)
        path: Option<String>,
        #[command(flatten)]
        filters: FilterArgs,
        /// Order of rows
        #[arg(long, value_enum, default_value_t = SortKey::Path)]
        sort: SortKey,
        /// Output format
        #[arg(long, value_enum, default_value_t = ExportFormat::Csv)]
        format: ExportFormat,
    },
    /// Inspect and maintain the index database
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
}

#[derive(Subcommand)]
pub(crate) enum DbAction {
    /// List indexed paths with file counts, duplicates, size and last run
    Info,
    /// Remove the index for a path (asks for confirmation)
    Drop {
        /// Indexed path to remove, as listed by `db info`
        #[arg(required_unless_present = "table")]
        path: Option<String>,
        /// Remove a table by name instead, e.g. one `db info` lists with no recorded path
        #[arg(long, conflicts_with = "path")]
        table: Option<String>,
        /// Don't ask for confirmation (required when not run from a terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Compact the database file after rows or tables have been removed
    Vacuum,
    /// Run SQLite's integrity check and report tables with no recorded path
    Check,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum ExportFormat {
    /// Comma-separated, with a header row
    Csv,
    /// Tab-separated, with a header row
    Tsv,
    /// One JSON object per line
    Json,
}

impl From<ExportFormat> for Format {
    fn from(f: ExportFormat) -> Self {
        match f {
            ExportFormat::Csv => Format::Csv,
            ExportFormat::Tsv => Format::Tsv,
            ExportFormat::Json => Format::Json,
        }
    }
}

/// "0.4.1" → "4.1": the project numbers releases v1, v2, … while the crate stays at 0.x
pub(crate) fn display_version() -> &'static str {
    env!("CARGO_PKG_VERSION").trim_start_matches("0.")
}
