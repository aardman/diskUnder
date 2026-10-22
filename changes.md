# diskUnder — Change Log

## Unreleased

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
