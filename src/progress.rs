//! Run state shared by both displays: counters, errors and live duplicate tracking

use std::collections::{HashMap, HashSet};

use crate::fmt::{fmt_bytes, fmt_count};
use crate::scan::{FileId, WorkerMsg};

pub(crate) struct DupGroup {
    pub(crate) size: u64,
    pub(crate) paths: Vec<String>,
    pub(crate) copies: usize, // distinct files; paths.len() − copies are extra hard links
    pub(crate) wasted: u64,   // size × (copies − 1)
}

/// Every path seen with one checksum. Paths sharing a FileId are hard links to the same
/// data, so they count as one copy; paths with no FileId each count as their own copy.
pub(crate) struct ChecksumGroup {
    pub(crate) size: u64,
    pub(crate) paths: Vec<String>,
    pub(crate) ids: HashSet<FileId>,
    pub(crate) unknown_ids: usize,
}

impl ChecksumGroup {
    pub(crate) fn copies(&self) -> usize {
        self.ids.len() + self.unknown_ids
    }
}

// ─── Application state ───────────────────────────────────────────────────────

pub(crate) struct AppState {
    pub(crate) mode: String,
    pub(crate) target_path: String,
    pub(crate) db_path: String,

    pub(crate) files_found: usize,
    pub(crate) files_skipped: usize, // already indexed (index mode) or unchanged (update mode)
    pub(crate) files_recorded: usize, // new or changed files whose details were written
    pub(crate) quick_checked: usize,  // quick hashes (size, first and last 64 KB)
    pub(crate) full_hashed: usize,    // files read in full
    pub(crate) unread: Option<usize>, // this path's files not read, once comparing is done
    pub(crate) stage: &'static str,   // "walk", then "record", "quick", "full"
    pub(crate) stage_total: usize,
    pub(crate) stage_done: usize,     // including files that couldn't be read
    pub(crate) files_written: usize,
    pub(crate) files_removed: usize, // rows dropped for deleted files (update mode)
    pub(crate) files_excluded: usize, // files skipped by the filters
    pub(crate) folders_excluded: usize, // folders the walk didn't enter
    pub(crate) rows_excluded_removed: usize, // rows dropped because their type is now excluded
    pub(crate) files_errored: usize,  // queued files that could not be read or hashed
    pub(crate) files_unwritten: usize, // hashed files lost to a failed batch commit
    pub(crate) other_errors: usize,   // traversal and database errors
    pub(crate) fatal: Option<String>, // set if the run stopped before indexing anything
    pub(crate) total_files: Option<usize>, // set once traversal is complete

    pub(crate) current_file: String,

    // In-memory checksum index for live duplicate detection.
    // For very large runs (millions of files) this will grow proportionally.
    pub(crate) checksum_map: HashMap<String, ChecksumGroup>,
    pub(crate) dup_keys: HashSet<String>,                         // checksums with 2+ distinct copies
    pub(crate) dups_dirty: bool,                                  // top_dups needs refreshing
    pub(crate) top_dups: Vec<DupGroup>,                           // top 10 by wasted bytes
    pub(crate) dup_wasted: u64,                                   // wasted bytes across all groups

    pub(crate) log: Vec<String>,
    pub(crate) done: bool,
}

impl AppState {
    pub(crate) fn new(mode: &str, target_path: &str, db_path: &str) -> Self {
        AppState {
            mode: mode.to_owned(),
            target_path: target_path.to_owned(),
            db_path: db_path.to_owned(),
            files_found: 0,
            files_skipped: 0,
            files_recorded: 0,
            quick_checked: 0,
            full_hashed: 0,
            unread: None,
            stage: "walk",
            stage_total: 0,
            stage_done: 0,
            files_written: 0,
            files_removed: 0,
            files_excluded: 0,
            folders_excluded: 0,
            rows_excluded_removed: 0,
            files_errored: 0,
            files_unwritten: 0,
            other_errors: 0,
            fatal: None,
            total_files: None,
            current_file: String::new(),
            checksum_map: HashMap::new(),
            dup_keys: HashSet::new(),
            dups_dirty: false,
            top_dups: Vec::new(),
            dup_wasted: 0,
            log: Vec::new(),
            done: false,
        }
    }

    pub(crate) fn skipped_label(&self) -> &'static str {
        if self.mode == "update" { "unchanged" } else { "already indexed" }
    }

    pub(crate) fn errors_total(&self) -> usize {
        self.files_errored + self.files_unwritten + self.other_errors
    }

    /// One-line summary of the run for the run log
    pub(crate) fn log_summary(&self) -> String {
        if let Some(fatal) = &self.fatal {
            return fatal.clone();
        }
        let mut summary = format!(
            "{} found, {} {}, {} written, {} removed, {}, {} duplicate groups, {} wasted",
            fmt_count(self.files_found as i64),
            fmt_count(self.files_skipped as i64),
            self.skipped_label(),
            fmt_count(self.files_written as i64),
            fmt_count(self.files_removed as i64),
            self.errors_summary(),
            fmt_count(self.dup_keys.len() as i64),
            fmt_bytes(self.dup_wasted)
        );
        if let Some(excluded) = self.excluded_line() {
            summary.push_str(&format!(", {}", excluded));
        }
        summary
    }

    /// "120 excluded by filters, 2 folders skipped, 3 rows removed as excluded" — None when the
    /// filters left nothing out
    pub(crate) fn excluded_line(&self) -> Option<String> {
        if self.files_excluded == 0 && self.folders_excluded == 0 && self.rows_excluded_removed == 0 {
            return None;
        }
        let mut line = format!("{} excluded by filters", fmt_count(self.files_excluded as i64));
        if self.folders_excluded > 0 {
            let folders = self.folders_excluded;
            line.push_str(&format!(", {} {} skipped", fmt_count(folders as i64), if folders == 1 { "folder" } else { "folders" }));
        }
        if self.rows_excluded_removed > 0 {
            line.push_str(&format!(", {} rows removed as excluded", fmt_count(self.rows_excluded_removed as i64)));
        }
        Some(line)
    }

    /// "compared: 4 read in full, 12 quick checks; 3,400 not read …" — None before comparing
    pub(crate) fn compared_line(&self) -> Option<String> {
        let unread = self.unread?;
        Some(format!(
            "compared: {} read in full, {} quick checks; {} not read (no other file shares their size or first and last 64 KB)",
            fmt_count(self.full_hashed as i64),
            fmt_count(self.quick_checked as i64),
            fmt_count(unread as i64)
        ))
    }

    /// e.g. "3 errors (2 unreadable, 1 not written)"; just "0 errors" when clean
    pub(crate) fn errors_summary(&self) -> String {
        let parts: Vec<String> = [
            (self.files_errored, "unreadable"),
            (self.files_unwritten, "not written"),
            (self.other_errors, "traversal/db"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{} {}", n, label))
        .collect();
        if parts.is_empty() {
            "0 errors".to_string()
        } else {
            format!("{} errors ({})", self.errors_total(), parts.join(", "))
        }
    }

    /// How far through the current stage. Unreadable files count as done — otherwise
    /// permission-denied files leave the bar stuck below 100% when the run is done.
    pub(crate) fn progress(&self) -> f64 {
        if self.stage_total > 0 { self.stage_done as f64 / self.stage_total as f64 } else { 0.0 }
    }

    /// What the run is doing, for the displays: "recording", "quick checks", "reading copies"
    pub(crate) fn stage_label(&self) -> &'static str {
        match self.stage {
            "walk" => "scanning",
            "record" => "recording",
            "quick" => "quick checks",
            _ => "reading possible copies",
        }
    }

    pub(crate) fn push_log(&mut self, msg: String) {
        self.log.push(msg);
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    pub(crate) fn record_hashed(&mut self, path: String, size: u64, checksum: String, id: Option<FileId>) {
        self.full_hashed += 1;
        self.stage_done += 1;
        self.current_file = path.clone();
        self.track_checksum(path, size, checksum, id);
    }

    pub(crate) fn track_checksum(&mut self, path: String, size: u64, checksum: String, id: Option<FileId>) {
        // Empty files all share one checksum and waste nothing — not worth tracking
        if size == 0 {
            return;
        }

        let add = |g: &mut ChecksumGroup, path: String| {
            g.paths.push(path);
            match id {
                Some(id) => {
                    g.ids.insert(id);
                }
                None => g.unknown_ids += 1,
            }
        };
        match self.checksum_map.get_mut(&checksum) {
            Some(group) => {
                add(group, path);
                if group.copies() >= 2 {
                    self.dups_dirty = true;
                    if !self.dup_keys.contains(&checksum) {
                        self.dup_keys.insert(checksum);
                    }
                }
            }
            None => {
                let mut group = ChecksumGroup {
                    size,
                    paths: Vec::new(),
                    ids: HashSet::new(),
                    unknown_ids: 0,
                };
                add(&mut group, path);
                self.checksum_map.insert(checksum, group);
            }
        }
    }

    /// Recompute the top-10 duplicate groups if anything changed since the last call.
    /// Called once per UI tick rather than per file, and only scans groups that have
    /// duplicates, so cost doesn't grow with the number of files hashed.
    pub(crate) fn refresh_top_dups(&mut self) {
        if !self.dups_dirty {
            return;
        }
        self.dups_dirty = false;

        let mut ranked: Vec<(u64, &String)> = self
            .dup_keys
            .iter()
            .map(|k| {
                let g = &self.checksum_map[k];
                (g.size * (g.copies() as u64 - 1), k)
            })
            .collect();
        self.dup_wasted = ranked.iter().map(|(w, _)| w).sum();
        let by_wasted = |a: &(u64, &String), b: &(u64, &String)| b.0.cmp(&a.0);
        if ranked.len() > 10 {
            ranked.select_nth_unstable_by(9, by_wasted);
            ranked.truncate(10);
        }
        ranked.sort_unstable_by(by_wasted);

        self.top_dups = ranked
            .into_iter()
            .map(|(wasted, k)| {
                let g = &self.checksum_map[k];
                DupGroup { size: g.size, paths: g.paths.clone(), copies: g.copies(), wasted }
            })
            .collect();
    }

    pub(crate) fn apply(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::FileFound => self.files_found += 1,
            WorkerMsg::FileRecorded { path } => {
                self.files_recorded += 1;
                self.stage_done += 1;
                self.current_file = path;
            }
            WorkerMsg::QuickChecked { path } => {
                self.quick_checked += 1;
                self.stage_done += 1;
                self.current_file = path;
            }
            WorkerMsg::Compared { unread } => {
                self.unread = Some(unread);
                if let Some(line) = self.compared_line() {
                    self.push_log(line);
                }
            }
            WorkerMsg::Stage { name, files } => {
                self.stage = name;
                self.stage_total = files;
                self.stage_done = 0;
                match name {
                    "quick" => self.push_log(format!("quick checks of {} files whose size another file shares", files)),
                    "full" => self.push_log(format!("reading {} possible copies in full", files)),
                    _ => {}
                }
            }
            WorkerMsg::FileHashed { path, size, checksum, file_id } => {
                self.record_hashed(path, size, checksum, file_id);
            }
            WorkerMsg::FileError { path, error } => {
                self.files_errored += 1;
                self.stage_done += 1;
                self.push_log(format!("warn: {} — {}", path, error));
            }
            WorkerMsg::Error { context, error } => {
                self.other_errors += 1;
                self.push_log(format!("warn: {} — {}", context, error));
            }
            WorkerMsg::Fatal { context, error } => {
                self.other_errors += 1;
                self.push_log(format!("error: {} — {}", context, error));
                self.fatal = Some(format!("{} — {}", context, error));
            }
            WorkerMsg::Info(line) => self.push_log(line),
            WorkerMsg::Seed(entries) => {
                for (path, size, checksum, file_id) in entries {
                    self.track_checksum(path, size, checksum, file_id);
                }
            }
            WorkerMsg::BatchWritten(n) => self.files_written += n,
            WorkerMsg::Excluded { files, folders } => {
                self.files_excluded = files;
                self.folders_excluded = folders;
                self.push_log(format!("{} files and {} folders excluded by the filters", files, folders));
            }
            WorkerMsg::RemovedExcluded(n) => {
                self.rows_excluded_removed = n;
                self.push_log(format!("removed {} rows whose type is now excluded", n));
            }
            WorkerMsg::Removed(n) => {
                self.files_removed = n;
                self.push_log(format!("removed {} rows for files no longer on disk", n));
            }
            WorkerMsg::BatchFailed { files, error } => {
                self.files_unwritten += files;
                self.push_log(format!("error: batch of {} files not written — {}", files, error));
            }
            WorkerMsg::TraversalComplete { to_hash, skipped } => {
                self.total_files = Some(to_hash);
                self.files_skipped = skipped;
                if skipped > 0 {
                    self.push_log(format!(
                        "traversal complete — {} files found, {} {}, {} to hash",
                        self.files_found, skipped, self.skipped_label(), to_hash
                    ));
                } else {
                    self.push_log(format!("traversal complete — {} files found", to_hash));
                }
            }
            WorkerMsg::Done => {
                self.done = true;
                self.refresh_top_dups();
                self.push_log(format!(
                    "done — {} written, {}, {} duplicate groups",
                    self.files_written,
                    self.errors_summary(),
                    self.dup_keys.len()
                ));
            }
        }
    }
}
