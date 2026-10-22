# diskUnder — Change Log

## v5.2 (part 1) — dry run

### `index --dry-run` / `update --dry-run`

Walks the tree and reports what the run would do, without hashing or writing anything: files
found, already indexed (or unchanged), would hash — with sizes, split into new and changed for
`update` — and, for `update`, rows that would be removed. The same rules and safeguards as a
real run apply: rows under folders that can't be read are kept, and nothing is removed when
the walk finds no files. A breakdown by type shows what would be hashed.

Nothing changes: no database is created, no migrations run (pending ones — a v3 table to
convert, a schema update — are noted, and converted rows count as already indexed), and dry
runs aren't logged. A database from a newer diskunder is refused, as in a real run.

Tests check that the dry run's prediction matches the real run that follows — resume after
losing rows, and an update with new, changed, deleted and renamed files — and that the
database and log are untouched. 61 tests in total (was 56).

---

## v5.1 — report

### `report`

Summaries of an index. With no options, an overview: totals, duplicates, files not modified in a
year, then the largest files, space by type, space by folder and the largest duplicate groups.
Sections can also be asked for individually and combined:

- `--top-files [N]` — the largest files
- `--by-type` — files, size and share of the space per extension
- `--by-directory [--depth N]` — files, size and share per folder, rolled up N levels below
  the path given (a folder inside an indexed path reports relative to that folder)
- `--stale AGE` — files not modified within AGE: the total and share of the space, which
  folders hold them, and the largest; `--stale-by access` uses access times instead
- `--duplicates` — groups by wasted space, each with its copies (first 5, `--all-paths` for all,
  hard links marked), and a summary over every matching group: groups, files that are copies,
  total wasted, largest group. No cost figures.

Duplicate filters: `--min-size`, `--min-copies` (hard links count once), `--min-wasted`. The
shared filters (`--ext`, `--under`, `--name`, …) select groups with at least one matching copy;
every copy is still listed.

`--limit` sets rows per section (default 20, 10 in the overview); `--format csv|tsv|json` works
for a single section. Read-only, like `search`. On a synthetic million-row index each section
takes 0.2–1.1 s and the full overview 2.8 s.

Sizes and counts in the overview, by-type and by-folder sections add up every path, so a
hard-linked file counts once per link; duplicate figures count hard links as one copy.

### Sizes of a terabyte or more show as TB

The size formatter stopped at GB ("23260.6 GB"); it now goes to TB everywhere — TUI, plain
output, `search`, `db info` and reports.

### Tests

56 in total (was 48): new end-to-end tests cover every report section, duplicate filters and
their group-level matching, hard links within groups, the summary, path capping, formats and
argument rules.

---

## v5.0 — search, export and a run log

First of the small v5 releases (plan: [v5-plan.md](v5-plan.md)).

### `search`

Query the index without re-indexing: `--name` (glob, `--ignore-case`), `--ext` (case-insensitive,
`none` for no extension), `--larger-than` / `--smaller-than` (`500MB`, `1.5GB`), `--older-than` /
`--newer-than` (`90d`, `6w`, `3m`, `1y`, or a date), `--not-accessed`, `--under PATH`, and
`--duplicate` (hard-link aware). The path argument can be an indexed path, a folder inside one,
or omitted to search every indexed path — which also finds duplicates across shares.

Output is a readable table by default (`--limit`, default 100; `--sort path|size|modified|name`;
`--long` for access/creation dates and checksum), or `--format csv|tsv|json`, `--paths`, or `-0`
for `xargs -0`. The table is the same whether or not output is piped; only data goes to stdout,
and notes ("index as of …", "… N more") go to stderr. Output stops quietly when a pipe closes
(`| head`).

### `export`

Every column (root, path, filename, directory, extension, size, modified, accessed, created,
checksum, device, inode) with no row limit, as CSV (default), TSV or newline-delimited JSON.
Dates are ISO 8601 UTC; rows are streamed, so a million-row export doesn't sit in memory
(1.4 s as CSV on a synthetic million-row index).

`search` and `export` never modify the database: they don't run migrations, aren't logged, and
read older layouts (v3 tables, mixed-case extensions) as they are.

### Run log

Commands that change a database (`index`, `update`, `db drop`, `db vacuum`) write `start`,
`error`, `info` and `end` lines to `<database>.log`: the command line, user, host and version;
errors with running it (traversal and database errors, failed batches, fatal errors — capped at
50 per run; per-file read errors stay as counts); and an end line with outcome, duration, exit
code and the run's summary. `--log FILE` and `--no-log` change or disable it. The log rotates to
`.log.1` at 10 MB, lines from concurrent runs never interleave, and a log that can't be written
never fails the run. `db drop` and `db vacuum` are only logged once they act — an unknown path or
a declined confirmation writes nothing.

### Fatal errors exit non-zero

A run that stops before indexing anything — the database can't be opened, a migration fails,
or the database is from a newer diskunder — now exits with status 1 (it exited 0), so cron and
scripts can tell. Per-file errors still exit 0.

### Schema versioning and lowercase extensions

Migrations are numbered with SQLite's `PRAGMA user_version`. Schema 1 stores file extensions
lowercase, so `JPG` and `jpg` are one type and the extension index is usable; `index` / `update`
convert existing rows once. A database with a higher version than the running diskunder
supports is refused rather than modified.

### Source split into modules

`src/main.rs` (1,764 lines) is now `cli`, `scan`, `db`, `progress`, `tui`, `plain`,
`maintenance`, `fmt`, plus the new `runlog`, `query`, `output` and `search`. Behaviour-neutral;
the existing tests passed unchanged across the split.

### Tests

48 in total (was 29): 3 unit tests for size, age and extension parsing, and new end-to-end
tests for schema migration, the run log, and `search` / `export` filters, scopes and formats.

---

## v4.1 — database maintenance

### `roots` table: which table is which

Table names are an MD5 of the indexed path and the path itself wasn't stored anywhere, so with
several shares in one database there was no way to tell tables apart, see when each was last
updated, or spot leftover tables. A `roots` table now records, per table, the canonical path,
first run, last run start and finish, and mode. It's written at the start and end of every run,
so a run that was interrupted is visible (`last_finished` is NULL). Existing tables are
recorded on their next run.

### `diskunder db` subcommands

- `db info` — every indexed path with file count and size, duplicate groups and wasted space
  (hard-link aware, computed from the whole table), last and first run in local time, and the
  table's size on disk. Tables with no recorded path (from before v4.1, or leftover v3
  spellings) are listed with the command to remove them.
- `db drop <PATH>` / `db drop --table <NAME>` — remove a path's index. Works for paths that no
  longer exist. Asks for confirmation; refuses without `--yes` when not run from a terminal.
- `db vacuum` — compact the file after rows or tables are removed (reports before → after).
- `db check` — `PRAGMA integrity_check` (exit code 2 on problems), plus tables with no
  recorded path and recorded paths whose table is missing.

None of them create a database if the `--db` path doesn't exist. Eight new tests cover them
(29 in total).

### Version shown in the TUI comes from Cargo.toml

The TUI header now reads its version from the crate version (`0.4.1` → `v4.1`) instead of a
hard-coded string.

---

## v4.0 — fixes

Resumability, change detection and performance fixes on top of v3.0. No new features.

### Resume skips already-indexed files

**Previous behaviour:** On restart, `index` mode re-hashed every file and relied on
`INSERT OR IGNORE` to avoid duplicate rows. Only the database writes were skipped, so resuming
an interrupted run on a large network share cost almost as much as starting again.

**Fix:** Before traversal, `index` mode loads the paths already in the table and skips those
files entirely. The TUI shows a "Skipped" count, and the plain-text summary reports
`N already indexed`.

### Update mode skips unchanged files

**Previous behaviour:** `update` mode re-hashed every file modified in the last 24 hours, even
if it had not changed since an earlier `update` run. Running it more often than daily (e.g.
hourly from cron) re-hashed the same files repeatedly.

**Fix:** A file in the 24-hour window is skipped if its size and modified time match the stored
row. New files and files whose size or modified time differ are hashed and upserted as before.
The skipped count is reported as `N unchanged`. Modified times are stored to the second, so an
edit that keeps the same size within the same second as the stored time is not detected.

### Duplicate tracking no longer slows down as the run grows

**Previous behaviour:** Every time a hashed file matched an earlier checksum, the top-10
duplicate list was rebuilt by scanning every file hashed so far and cloning the path lists of
every duplicate group. Run time grew roughly with the square of the file count: 50,000 small,
duplicate-heavy files took 15.5 s and 150,000 took 230 s. Empty files made this worse, since
they all share one checksum.

**Fix:** The top-10 list is recomputed at most once per UI tick (100 ms) and once at the end,
only over checksums that actually have duplicates, and only the 10 groups displayed have their
paths cloned. Empty files are no longer tracked as duplicates (they waste no space). The same
test now takes 1.6 s and 4.6 s respectively.

### Accurate counts

**Previous behaviour:**
- Directory traversal errors were counted as file errors, but those entries were never part
  of the file total — so the progress bar could pass 100%.
- A failed batch commit counted as 1 error even though up to 1,000 files went unwritten.
- The "duplicate groups" figure and "wasted" total only covered the top 10 groups shown,
  and the TUI footer mislabelled that top-10 sum as "all groups".

**Fix:** Errors are tracked as three kinds — unreadable files (count toward progress), files
lost to a failed batch, and traversal/database errors (do not count toward progress). The
summary reports the total with a breakdown, e.g. `504 errors (1 unreadable, 502 not written,
1 traversal/db)`. Duplicate group count and wasted bytes now cover every group found; the TUI
footer reads `top 10 of N groups — X wasted across all groups`. Failure to create the
secondary indexes at the end of a run is now reported instead of silently ignored.

### Update mode catches every change and removes deleted files

**Previous behaviour:** `update` only looked at files modified in the last 24 hours. It
missed changes if it wasn't run daily, missed files copied with their original modified
time preserved (`rsync -a`, `cp -p`), never removed rows for deleted or renamed files, and
never created the secondary indexes if it was the first run against a table.

**Fix:** `update` now walks the whole tree and compares every file with the database. Files
that are new or whose size or modified time differ are hashed and upserted; the rest are
skipped as unchanged. Rows for paths not seen on disk are removed, with two safeguards:
- rows under a directory that could not be read are kept (a network blip doesn't wipe them)
- if the walk finds no files at all, nothing is removed — this is usually an unmounted share

Secondary indexes are now created at the end of every run (`IF NOT EXISTS`). The summary
reports `N removed`. `index` mode never removes rows.

### One table per directory, however the path is typed

**Previous behaviour:** The table name was the MD5 of the path exactly as typed, so `/nas`,
`/nas/`, `./nas` and a symlink to it each got a separate table (and a full re-hash). Stored
paths kept the typed spelling, including relative paths that only made sense from the
directory the tool was run in.

**Fix:** The root is canonicalised (absolute, symlinks resolved) before use, so all spellings
share one table and stored paths are always absolute.

**Migration:** If a table exists for the path as typed but not for the canonical path, its
paths and `directory` values are rewritten onto the canonical root and it is renamed, in a
single transaction — no re-hashing. Old-named indexes are dropped and recreated. If any row
can't be mapped, the transaction is rolled back and the run stops with an error rather than
re-hashing into a new table. Only the spelling used on the first v4 run is migrated.

### Duplicates found across the whole table

**Previous behaviour:** Duplicate detection only saw files hashed in the current run. Once
resume and `update` started skipping already-indexed files, a resumed run or an `update`
reported only duplicates among the few files it re-hashed — a re-run of a fully indexed share
reported none.

**Fix:** Files skipped as already indexed (`index`) or unchanged (`update`) contribute their
stored checksum to duplicate detection, so the live panel and totals cover every file in the
table that is still on disk. Rows for files no longer on disk are not counted.

### `change_date` column renamed to `birth_date`

**Previous behaviour:** The `change_date` column was filled from `metadata.created()` — the
file's birth (creation) time — not the inode change time (`ctime`) its name suggests.

**Fix:** The column is now `birth_date`. Existing tables are renamed in place on their next run
(`ALTER TABLE … RENAME COLUMN`, no data rewritten, values kept). Queries that select
`change_date` need updating. `setupDB.sh` uses the new name too.

### Hard links are not counted as wasted space

**Previous behaviour:** Every path with a matching checksum counted as a separate copy, so
hard links — several paths to the same data on disk — were reported as duplicates wasting
space, even though removing one frees nothing.

**Fix:** Each file's device and inode are recorded in new `device` and `inode` columns.
Within a duplicate group, paths sharing a device and inode count as one copy; wasted space is
`size × (distinct copies − 1)`, and a group made only of hard links is not a duplicate. The
TUI shows e.g. `1 KB × 2 copies (+1 hard link)`.

Existing tables get the columns added automatically. `update` fills them in for unchanged
rows without re-hashing; `index` does not (it doesn't read file metadata for rows it skips),
so until an `update` runs, older rows fall back to counting every path. Over SMB, hard links
usually aren't visible as such, so this matters most when running on the file server itself.

### Automated tests

**Previous behaviour:** No tests; changes were checked by hand.

**Fix:** `tests/cli.rs` — 21 end-to-end tests that run the built binary against temporary
directories and check its output and database: resume, update change detection (including
back-dated edits), deletions and their safeguards, path normalisation, v3 table migration and
its failure path, error counts by kind, duplicate totals, hard links, inode backfill and
argument parsing. Run with `cargo test`.

### `structopt` replaced with `clap` v4

`structopt` is in maintenance mode (it was merged into `clap`). The CLI now uses `clap`'s
derive API. Commands, flags and defaults are unchanged, and global flags still work before or
after the subcommand. `--version` now works, and `--help` describes the `PATH` argument. This
also drops `atty`, an unmaintained dependency of `structopt` with a known advisory.

### `setupDB.sh` removed

It created a single static `files` table that diskunder never uses — the tool creates its own
per-path tables on first run — so it suggested a setup step that doesn't exist. The README now
has an Install section instead.

### Plain mode prints traversal and database warnings

Plain mode previously printed no warnings at all, so a failure showed only as an error count.
Traversal and database errors (and informational lines such as migrations) are now printed to
stderr. Per-file read errors are still only counted.

---

## v3.0 — `diskUnder3.rust`

Added a full-screen TUI (terminal UI) built with [ratatui](https://ratatui.rs) + crossterm.
Requires two new Cargo.toml entries: `ratatui = "0.28"` and `crossterm = "0.28"`.
All resilience and DB fixes from v2.0 are carried forward.

### Bug fixes

**Progress bar stuck below 100% on permission-restricted paths (e.g. `/Users/`)**
`progress()` calculated `files_hashed / total_files`. On macOS `/Users/` (or any path with
protected system files), a large fraction of files fail with permission denied and increment
`files_errored` but not `files_hashed`, leaving the bar stuck at the error fraction even
when the run was complete. Fixed to `(files_hashed + files_errored) / total_files` so every
processed file — success or error — advances the bar.

### New features

**Live progress panel**
Shows files found, hashed, written to DB, and error count — updated in real time as each
batch completes. A progress bar fills as hashing progresses toward the total found during
traversal. The current file being hashed scrolls at the bottom of the panel.

**Live duplicate detection panel**
As each file is hashed, its checksum is recorded in an in-memory map. Whenever a checksum
matches a previously seen file, a duplicate group is created or updated. The top 10 groups
by wasted bytes are displayed live and resort as new duplicates are found. Up to 3 paths
are shown per group; larger groups show a count of remaining paths. A footer line totals
the wasted bytes across all displayed groups.

**Activity log**
A scrolling panel at the bottom shows traversal completion, batch commits, warnings, and
errors. Errors and warnings are highlighted in red; completion messages in green.

**Plain-text summary on exit**
After the TUI closes (press `q` or `Esc`), a compact summary is printed to stdout:
files found, files written, error count, duplicate group count, and total wasted bytes.

**`--db` and `--threads` flags**
Carried forward from v2.0 — database path and thread count are both configurable via CLI.

---

## v2.0 — `diskUnder2.rust`

Rewrote the Rust implementation to address network share resilience, resumability after failure, and database integrity issues present in v1.0.

---

### 1. Streaming file hash (resilience — large files / network drops)

**File:** `diskUnder2.rust` — `hash_file_streaming()`

**v1.0 behaviour:** `fs::read(path)` loaded the entire file into RAM before hashing. A 10 GB file would exhaust memory; a network dropout mid-read silently discarded the file via `.ok()`.

**v2.0 fix:** Replaced with a `BufReader` feeding a `blake3::Hasher` in 64 KB chunks. Memory use is constant regardless of file size, and a read error produces a recoverable warning rather than a silent drop.

---

### 2. WalkDir errors logged, not swallowed (resilience — network traversal)

**File:** `diskUnder2.rust` — `collect_all_files()`, `collect_recent_files()`

**v1.0 behaviour:** `.filter_map(|e| e.ok())` silently converted every WalkDir error to `None`. A network share going offline mid-traversal produced a truncated file list with no warning and no error exit code.

**v2.0 fix:** Replaced with an explicit `match` that prints each error to stderr and tracks a total. A final warning is printed if any entries could not be traversed.

---

### 3. Batched transactions — resumability after crash or network drop

**File:** `diskUnder2.rust` — `commit_batch()`, `process_files()`

**v1.0 behaviour:** All files were hashed into memory first, then written in a single database transaction. If anything failed (network drop, OOM, crash), zero rows were committed and the entire run had to restart from scratch.

**v2.0 fix:** Files are processed in 1,000-file batches (`BATCH_SIZE`). Each batch is committed as its own transaction. On restart, already-committed paths are skipped via `INSERT OR IGNORE` on the `path UNIQUE` constraint, so the run resumes roughly where it left off.

---

### 4. Proper upsert — DB integrity on re-index

**File:** `diskUnder2.rust` — `commit_batch()`

**v1.0 behaviour:** `INSERT OR REPLACE` is not a true upsert — it deletes the old row and inserts a new one, destroying the original `created_at` timestamp and resetting the `id`.

**v2.0 fix:**
- **Index mode:** `INSERT OR IGNORE` — leaves existing rows untouched (supports resuming).
- **Update mode:** `INSERT ... ON CONFLICT(path) DO UPDATE SET ...` — updates only the changed columns in-place, preserving `created_at` and the row `id`.

---

### 5. No `unwrap()` on `modified()` in update filter (panic fix)

**File:** `diskUnder2.rust` — `collect_recent_files()`

**v1.0 behaviour:** `m.modified().unwrap()` panicked on filesystems that do not support modification time (FAT volumes, some NFS mounts).

**v2.0 fix:** `.and_then(|m| m.modified().ok())` — files with unreadable mtimes are skipped with a stderr warning rather than panicking.

---

### 6. `metadata.created()` no longer silently drops files on Linux/NFS

**File:** `diskUnder2.rust` — `FileEntry::from_path()`

**v1.0 behaviour:** `change_date` used `metadata.created()?`. On Linux and most NFS mounts, `created()` (birth time) is unsupported and returns `Err`. The `?` propagated this error, causing `from_path` to fail and `.ok()` to silently discard the file. This meant every file on a Linux-hosted share was silently skipped.

**v2.0 fix:** Wrapped in `.ok().map(...).unwrap_or_default()` — stores an empty string when birth time is unavailable rather than failing the file entry.

---

### 7. Rayon thread count limited for network shares

**File:** `diskUnder2.rust` — `main()`

**v1.0 behaviour:** Rayon defaulted to one thread per CPU core. Against an SMB/NFS server this opened many simultaneous connections, causing servers to reject or rate-limit requests — which again silently disappeared via `.ok()`.

**v2.0 fix:** Default is 4 threads, configurable with `--threads N`. Raise for fast local storage; keep low for network shares.

---

### 8. Configurable database path (`--db`)

**File:** `diskUnder2.rust` — `main()`

**v1.0 behaviour:** Database was hardcoded as `"fileindex.db"` relative to the current working directory. Running from different directories created different databases.

**v2.0 fix:** `--db <path>` argument with default `fileindex.db`. Can be set to an absolute path for consistent behaviour regardless of CWD.

---

### 9. Path existence check before touching the database

**File:** `diskUnder2.rust` — `main()`

**v1.0 behaviour:** If the target path did not exist (e.g. unmounted share, typo), the tool created an empty table in the database and exited successfully with no output.

**v2.0 fix:** Exits immediately with an error message if the path is not found, before any database writes occur.

---

## v1.0 — `diskUnder.rust`

Original Rust implementation. Functional on local filesystems for small-to-medium directory trees. See issues above for limitations on network shares and large datasets.
