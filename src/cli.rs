//! Command-line arguments

use clap::{Parser, Subcommand};

use crate::output::Format;
use crate::filters::{parse_path_pattern, Overrides, PathFilter, TypeFilter};
use crate::query::{parse_ext_list, parse_size, ExtList, FilterArgs, SortKey};
use crate::report::{parse_age_arg, Age, StaleBy};

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
        /// Show what would be indexed, without hashing or writing anything
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        types: TypeArgs,
    },
    /// Re-index new and changed files under PATH, and drop rows for deleted files
    Update {
        /// Directory to update
        path: String,
        /// Show what would be hashed and removed, without hashing or writing anything
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        types: TypeArgs,
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
    /// Summaries: largest files, space by type and folder, stale files, duplicate groups
    Report {
        /// Indexed path, or a folder inside one (default: every indexed path)
        path: Option<String>,
        #[command(flatten)]
        filters: FilterArgs,
        /// Largest files (optionally how many, e.g. --top-files 50)
        #[arg(long, value_name = "N", num_args = 0..=1)]
        top_files: Option<Option<usize>>,
        /// Space used per file type
        #[arg(long)]
        by_type: bool,
        /// Space used per folder
        #[arg(long)]
        by_directory: bool,
        /// Folder levels below the path for --by-directory and --stale
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
        depth: u16,
        /// Files not modified within AGE, e.g. 180d, 1y: total, by folder, largest
        #[arg(long, value_name = "AGE", value_parser = parse_age_arg)]
        stale: Option<Age>,
        /// What --stale measures
        #[arg(long, value_enum, default_value_t = StaleBy::Modified, requires = "stale")]
        stale_by: StaleBy,
        /// Duplicate groups, most wasted space first, with a summary
        #[arg(long)]
        duplicates: bool,
        /// Only groups whose files are at least SIZE
        #[arg(long, value_name = "SIZE", value_parser = parse_size, requires = "duplicates")]
        min_size: Option<u64>,
        /// Only groups with at least N distinct copies (hard links count once)
        #[arg(long, value_name = "N", default_value_t = 2, requires = "duplicates")]
        min_copies: usize,
        /// Only groups wasting at least SIZE in total
        #[arg(long, value_name = "SIZE", value_parser = parse_size, requires = "duplicates")]
        min_wasted: Option<u64>,
        /// List every path in each duplicate group (default: first 5)
        #[arg(long, requires = "duplicates")]
        all_paths: bool,
        /// Rows per section (default 20, or 10 in the overview); 0 for all
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Output format — csv, tsv and json need a single section
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
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

/// Filters for `index` and `update`. Type options replace the stored type filters and path
/// options the stored path filters; whatever isn't given is reused from the last run.
#[derive(clap::Args)]
pub(crate) struct TypeArgs {
    /// Only index these extensions, comma-separated, e.g. jpg,png,mov ("none" = no extension)
    #[arg(long, value_name = "LIST", value_parser = parse_ext_list)]
    pub(crate) include_ext: Option<ExtList>,

    /// Don't index these extensions, e.g. tmp,log
    #[arg(long, value_name = "LIST", value_parser = parse_ext_list)]
    pub(crate) exclude_ext: Option<ExtList>,

    /// Clear any stored file-type filters and index every type
    #[arg(long, conflicts_with_all = ["include_ext", "exclude_ext"])]
    pub(crate) all_types: bool,

    /// Skip paths matching PATTERN, .gitignore-style and case-sensitive: a name like .snapshot
    /// or *.tmp matches at any depth; a/b/c is from the indexed root; ** spans folders (repeatable)
    #[arg(long, value_name = "PATTERN", value_parser = parse_path_pattern)]
    pub(crate) exclude_path: Vec<String>,

    /// Index paths matching PATTERN even if an --exclude-path matches them — exceptions only
    #[arg(long, value_name = "PATTERN", value_parser = parse_path_pattern)]
    pub(crate) include_path: Vec<String>,

    /// Clear any stored path filters
    #[arg(long, conflicts_with_all = ["exclude_path", "include_path"])]
    pub(crate) no_path_filters: bool,
}

impl TypeArgs {
    /// The filters given on the command line; a kind not given is None (reuse the stored one)
    pub(crate) fn overrides(&self) -> Overrides {
        let types = if self.all_types {
            Some(TypeFilter::default())
        } else if self.include_ext.is_some() || self.exclude_ext.is_some() {
            Some(TypeFilter {
                include: self.include_ext.as_ref().map(|l| l.0.clone()),
                exclude: self.exclude_ext.as_ref().map(|l| l.0.clone()).unwrap_or_default(),
            })
        } else {
            None
        };
        let paths = if self.no_path_filters {
            Some(PathFilter::default())
        } else if !self.exclude_path.is_empty() || !self.include_path.is_empty() {
            // Already checked by parse_path_pattern
            PathFilter::new(self.exclude_path.clone(), self.include_path.clone()).ok()
        } else {
            None
        };
        Overrides { types, paths }
    }
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
