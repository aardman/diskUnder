# diskUnder

A command-line tool for indexing file metadata and detecting duplicate files/folders across a filesystem or network share — a CLI equivalent of [DiskOver](https://diskoverdata.com/). File metadata is stored in a local SQLite database, enabling fast queries for duplicates, large files, and directory summaries.

---

## How It Works

diskUnder walks a target directory, collects file metadata (size, dates, BLAKE3 checksum), and writes it to a SQLite database. Multiple paths can be indexed into the same database — each path gets its own table named `files_<MD5(path)>`, where `path` is the canonical (absolute, symlink-resolved) root. `/nas`, `/nas/`, `./nas` and a symlink to it all share one table, and stored paths are always absolute.

Two modes are supported:

- **`index`** — full traversal of the target path. Files already in the database are skipped without being re-hashed, so an interrupted run can simply be restarted
- **`update`** — full traversal that brings the table in line with disk: new files and files whose size or modified time differ from the database are hashed, unchanged files are skipped without being re-hashed, and rows for files no longer on disk are removed. Rows under a directory that could not be read are kept, and nothing is removed if the walk finds no files at all (e.g. an unmounted share)

---

## Install

The binaries are self-contained — no runtime dependencies, and the database is created on
first run, so installing is just copying the right one onto your `PATH` as `diskunder`:

**macOS (Apple Silicon):**

```bash
sudo mkdir -p /usr/local/bin
sudo install -m 755 diskunder-macos /usr/local/bin/diskunder
```

**Linux (x86_64 — statically linked, any distribution):**

```bash
sudo install -m 755 diskunder-linux /usr/local/bin/diskunder
```

**Or without sudo, for your user only** — make sure `~/.local/bin` is on your `PATH`, and use
`diskunder-macos` instead on a Mac:

```bash
mkdir -p ~/.local/bin
install -m 755 diskunder-linux ~/.local/bin/diskunder
```

**Check it worked** — this should print `diskunder 0.5.2`:

```bash
diskunder --version
```

- **macOS:** if you downloaded the binary through a browser rather than cloning the repo,
  Gatekeeper will block it as unverified. Clear the quarantine flag once with
  `xattr -d com.apple.quarantine diskunder-macos`.
- **Other platforms** (Intel Mac, ARM Linux): build from source — see
  [Building the binaries](#building-the-binaries).
- **Uninstall:** delete the binary. Index databases are wherever you pointed `--db`
  (default `fileindex.db` in the directory you ran it from).

---

## Usage

Displays a live full-screen terminal UI with real-time progress, duplicate detection, and an
activity log. Falls back to plain text automatically when stdout is not a terminal (cron, SSH,
pipes).

Examples below assume it's installed as `diskunder` (see [Install](#install)); otherwise run `./diskunder-macos` or `./diskunder-linux` from the repository root.

```text
diskunder index <PATH> [OPTIONS]
diskunder update <PATH> [OPTIONS]
```

**Options:**

| Flag | Default | Description |
|---|---|---|
| `--db <FILE>` | `fileindex.db` | SQLite database path (created if absent) |
| `--threads <N>` | `4` | Parallel hashing threads — reduce for network shares |
| `--no-tui` | off | Disable TUI and print plain progress lines to stderr |

**`--no-tui` / auto-detection:**
The TUI is skipped automatically when stdout is not an interactive terminal, so no flag is
needed for cron jobs, piped output, or SSH sessions without a TTY. Use `--no-tui` to force
plain mode explicitly in an interactive terminal.

Interactive, with the full TUI:

```bash
diskunder index /Volumes/NAS --db /data/index.db --threads 4
```

Headless or scripted — plain text is used automatically, and progress goes to stderr:

```bash
diskunder index /Volumes/NAS --db /data/index.db 2>&1 | tee run.log
```

Plain text in an interactive terminal:

```bash
diskunder --no-tui index /Volumes/NAS --db /data/index.db
```

Update mode — pick up new, changed and deleted files:

```bash
diskunder update /Volumes/NAS --db /data/index.db
```

**File types.** Index only the types you care about, or skip ones you don't — excluded files
are never read, so runs get faster too. Extensions are matched case-insensitively on the text
after the last dot (`archive.tar.gz` → `gz`); `none` means files without one:

```bash
diskunder index /Volumes/Photos --include-ext jpg,jpeg,png,heic,mov,mp4 --db /data/index.db
diskunder index /Volumes/NAS --exclude-ext tmp,log,ds_store --db /data/index.db
```

The filters are stored with the path, so a later `update` with no filters uses them again (and
says so). Passing `--include-ext` or `--exclude-ext` replaces them; `--all-types` clears them.
When a type becomes excluded, `update` removes its rows and reports them separately from
deleted files (`index` never removes rows — it notes how many `update` would remove).
`db info` shows each path's filters, and `search` and `report` note when an index is filtered.

Preview a run with `--dry-run`: it walks the tree and reports what `index` or `update` would
do, using the same rules and safeguards, but hashes and writes nothing — no database is
created, nothing is migrated, and it isn't logged. Useful before a long first index, or to
see how much has changed since the last `update`:

```bash
diskunder update /Volumes/NAS --db /data/index.db --dry-run
```

```text
Dry run — update /Volumes/NAS (nothing hashed or written)

  Found            772,087 files · 4.1 TB
  Unchanged        771,950 files
  Would hash       137 files · 1.2 GB  (90 new, 47 changed)
  Would remove     12 rows (no longer on disk)
  Couldn't read    1 folder — rows under it are kept

Would hash, by type
  TYPE  FILES     SIZE  SHARE
  mov      12   1.1 GB  91.0%
  …
```

**Plain text output format:**
```
index /Volumes/NAS
  1000/772087 processed — 998 written, 2 errors
  2000/772087 processed — 1991 written, 9 errors
  ...
complete: 772087 files — 0 already indexed, 749404 written, 0 removed, 22683 errors (22683 unreadable), 1408 duplicate groups
wasted: 3.2 GB
```

**Dependencies:** `blake3`, `chrono`, `clap`, `crossterm`, `libc`, `md5`, `rayon`, `ratatui`, `rusqlite`, `walkdir`

---

## Try it with sample data

A two-minute walkthrough on a throwaway folder — nothing outside `/tmp/diskunder-demo` is
touched. Run it from the repository root. Steps 2–4 use `--no-tui` so your output matches
what's shown here; leave it off to watch the live TUI instead (press `q` when it finishes).

**1. Make some fake data** — three 200 KB "photos", a backup folder holding two real copies
and one hard link, and a small text file. On MacOS, change the first line to
`BIN=$PWD/diskunder-macos`; if you've installed it, `BIN=diskunder` works on either.

```bash
BIN=$PWD/diskunder-linux
DEMO=/tmp/diskunder-demo
rm -rf "$DEMO" && mkdir -p "$DEMO/data/photos" "$DEMO/data/backup"

for i in 1 2 3; do head -c 200000 /dev/urandom > "$DEMO/data/photos/img$i.jpg"; done
cp "$DEMO/data/photos/img1.jpg" "$DEMO/data/photos/img2.jpg" "$DEMO/data/backup/"
ln "$DEMO/data/photos/img3.jpg" "$DEMO/data/backup/img3-link.jpg"
echo "shopping list" > "$DEMO/data/notes.txt"
```

**2. Index it:**

```bash
$BIN index "$DEMO/data" --db "$DEMO/demo.db" --no-tui
```
```
index /private/tmp/diskunder-demo/data
  7/7 processed — 7 written, 0 errors
complete: 7 files — 0 already indexed, 7 written, 0 removed, 0 errors, 2 duplicate groups
wasted: 390.6 KB
```

Two duplicate groups (`img1` and `img2` each have a copy in `backup/`), wasting 2 × 200 KB.
The hard link `img3-link.jpg` is not counted — it's the same data on disk, so deleting it
would free nothing. (On Linux the path shows as `/tmp/…`; macOS resolves `/tmp` to `/private/tmp`.)

**3. Run it again** — everything is already indexed, so nothing is re-hashed:

```bash
$BIN index "$DEMO/data" --db "$DEMO/demo.db" --no-tui
```
```
index /private/tmp/diskunder-demo/data
complete: 7 files — 7 already indexed, 0 written, 0 removed, 0 errors, 2 duplicate groups
wasted: 390.6 KB
```

**4. Clean up:**

```bash
rm -rf /tmp/diskunder-demo
```

---

## Searching and exporting

`search` and `export` read the index — they never touch the files themselves, so they're
instant even on a share that's slow or offline. Run `update` first for current answers; both
commands start by noting how old the index is.

Give an indexed path, a folder inside one, or nothing to search every indexed path (which
also finds duplicates *across* shares):

```bash
diskunder search /Volumes/NAS --ext mov,mp4 --larger-than 1GB
diskunder search /Volumes/NAS/projects --older-than 1y --sort size
diskunder search --duplicate --larger-than 500MB
diskunder search /Volumes/NAS --name "*.log" --ignore-case --paths
```

```text
index as of 2026-10-23 02:14
     SIZE  MODIFIED          PATH
   2.9 GB  2024-11-01 10:22  /Volumes/NAS/backup/render.mov
   2.9 GB  2024-11-01 10:22  /Volumes/NAS/projects/film one/render.mov
```

| Filter | Matches |
|---|---|
| `--name GLOB` | file name, e.g. `"*.log"` (`--ignore-case` to ignore case) |
| `--ext LIST` | extensions, case-insensitive, e.g. `jpg,mov`; `none` for files without one |
| `--larger-than SIZE` / `--smaller-than SIZE` | `500MB`, `1.5GB`, `200K` (1024-based) |
| `--older-than AGE` / `--newer-than AGE` | modified date: `90d`, `6w`, `3m`, `1y`, or `2024-01-01` |
| `--not-accessed AGE` | access date (unreliable on many servers) |
| `--under PATH` | only files under this folder |
| `--duplicate` | files with at least one other copy (hard links don't count) |

**Output.** A readable table by default, the same whether or not it's piped. Only data goes
to stdout — notes such as "index as of …" and "… 340 more" go to stderr, so `| grep` and
`> file` get clean rows.

| Option | Effect |
|---|---|
| `--limit N` | rows to show (default 100; `0` for all) |
| `--sort path\|size\|modified\|name` | order (size and modified are largest/newest first) |
| `--long` | add access and creation dates and checksum to the table |
| `--format csv\|tsv\|json` | machine-readable, every column; JSON is one object per line |
| `--paths` / `-0` | bare paths, one per line or NUL-separated for `xargs -0` |

**`export`** is `search` for spreadsheets and BI tools: every column, no row limit, CSV by
default (`--format tsv` or `json` also work), streamed so a million rows don't sit in memory.

```bash
diskunder export /Volumes/NAS > nas.csv
diskunder export /Volumes/NAS --ext mov,mp4 --format json > video.ndjson
```

Columns: `root, path, filename, directory, extension, size, modified, accessed, created,
checksum, device, inode`. Dates are ISO 8601 in UTC (`2024-11-01T10:22:05Z`); sizes are bytes.

---

## Reports

`report` summarises an index. With no options it prints an overview — totals, duplicates,
files not modified in a year, then the largest files, space by type, space by folder and the
largest duplicate groups:

```bash
diskunder report /Volumes/NAS
```

```text
  Files                  772,087 · 4.1 TB
  Duplicates             1,408 groups · 3.2 GB wasted
  Not modified in 1y     210,334 files · 1.9 TB (46.3% of the space)

Largest files
     SIZE  MODIFIED          PATH
   1.6 GB  2024-11-01 10:22  /Volumes/NAS/projects/2024/final/render.mov
…
```

Or ask for specific sections (they can be combined):

| Section | Shows |
|---|---|
| `--top-files [N]` | the largest files (default 20) |
| `--by-type` | files, size and share of the space per extension |
| `--by-directory [--depth N]` | files, size and share per folder, rolled up N levels below the path (default 1) |
| `--stale AGE` | files not modified within AGE (`180d`, `1y`): the total, which folders hold them, and the largest. `--stale-by access` uses access times instead — unreliable on many file servers |
| `--duplicates` | duplicate groups, most wasted space first, each listing its copies (first 5; `--all-paths` for all, hard links marked), then a summary |

```bash
diskunder report /Volumes/NAS --by-type --by-directory --depth 2
diskunder report /Volumes/NAS/projects --stale 1y
diskunder report /Volumes/NAS --duplicates --min-size 1GB --min-copies 3
diskunder report --duplicates --ext iso,dmg,vhd
```

**Duplicate filters:** `--min-size SIZE` (each copy at least SIZE), `--min-copies N` (distinct
copies, hard links counting once), `--min-wasted SIZE` (size × (copies − 1)). The shared
filters from `search` (`--ext`, `--under`, `--name`, …) pick groups with **at least one**
matching copy, and every copy is still listed — so `--under /Volumes/NAS/backup` shows each
backup copy alongside the original elsewhere. The summary covers every matching group, even
when `--limit` shows fewer:

```text
  Duplicate groups       : 47
  Files that are copies  : 183
  Total wasted space     : 142.3 GB
  Largest group          : render.mov — 1.6 GB × 4 copies (4.8 GB wasted)
```

**Options:** `--limit N` rows per section (default 20, or 10 in the overview; `0` for all),
and `--format csv|tsv|json` for a single section — e.g. `--by-type --format csv` for a
spreadsheet. Like `search`, notes go to stderr and reports never change the database.

Sizes and file counts add up every path, so a hard-linked file counts once per link;
duplicate groups and wasted space count hard links as one copy.

---

## Database maintenance

`diskunder db` looks after the index database. It never creates a database — pass the same
`--db` you index with (default `fileindex.db`).

| Command | What it does |
|---|---|
| `diskunder db info` | Lists every indexed path: file count and total size, duplicate groups and wasted space, last run (mode, start, whether it finished), first run, and the table's size on disk |
| `diskunder db drop <PATH>` | Removes the index for a path — e.g. a share you no longer track. Works even if the path no longer exists. Asks for confirmation; pass `--yes` in scripts |
| `diskunder db drop --table <NAME>` | Removes a table by name — for tables `db info` lists with no recorded path |
| `diskunder db vacuum` | Compacts the file. SQLite reuses space freed by `update` and `drop` but doesn't shrink the file until vacuumed |
| `diskunder db check` | Runs SQLite's integrity check (exit code 2 if it finds problems) and lists tables with no recorded path |

```bash
diskunder db info   --db /data/index.db
diskunder db drop   /Volumes/OldShare --db /data/index.db
diskunder db vacuum --db /data/index.db
diskunder db check  --db /data/index.db
```

Times are stored in UTC and shown in local time. A last run that "did not finish" was
interrupted (or is still running); re-running it resumes where it stopped. Back up the
database with SQLite's online backup while nothing is writing to it:
`sqlite3 /data/index.db ".backup /backups/index.db"`.

### Database Schema

```sql
CREATE TABLE files_<MD5_HASH> (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT NOT NULL UNIQUE,
    filename      TEXT NOT NULL,
    size          INTEGER NOT NULL,
    modified_date TEXT,
    access_date   TEXT,
    birth_date    TEXT,       -- creation time where the filesystem records one, else empty
    checksum      TEXT,       -- BLAKE3
    extension     TEXT,
    directory     TEXT,
    created_at    TEXT DEFAULT CURRENT_TIMESTAMP,
    device        INTEGER,    -- device + inode identify hard links to the same data
    inode         INTEGER
);
```

Indexes are created on `size`, `filename`, `checksum`, `extension`, `directory`, `access_date`, and `modified_date`.

Since table names are a hash, a `roots` table (v4.1+) records which path each table belongs to
and how its last run went. It is updated at the start and end of every run:

```sql
CREATE TABLE roots (
    table_name    TEXT PRIMARY KEY,   -- files_<MD5_HASH>
    root          TEXT NOT NULL,      -- canonical path
    first_run     TEXT NOT NULL,      -- UTC
    last_started  TEXT NOT NULL,
    last_finished TEXT,               -- NULL if the last run was interrupted (or is still running)
    last_mode     TEXT NOT NULL,      -- index | update
    include_ext   TEXT,               -- file-type filters: comma-separated, "none" = no extension
    exclude_ext   TEXT
);
```

The layout version is kept in SQLite's `PRAGMA user_version` (v5.2+: `2`). `index` and
`update` bring an older database up to date; a database written by a newer diskunder is
refused rather than modified.

### SQLite Optimisations

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = 100000;
```

---

## Run log

Every command that changes a database — `index`, `update`, `db drop`, `db vacuum` — is
recorded in `<database>.log` next to it (`fileindex.db.log` by default): what was run, by whom,
how it ended, a one-line summary, and any errors with running it. Per-file read errors are
counted in the summary, not listed. Read-only commands (`search`, `export`, `db info`,
`db check`) aren't logged.

```text
2026-10-23T02:00:01+01:00 run=7f3a2c start  update /Volumes/NAS --db /data/index.db  (user=svc-diskunder host=nas-util01 version=5.0)
2026-10-23T02:00:09+01:00 run=7f3a2c error  /Volumes/NAS/Finance — IO error for operation on /Volumes/NAS/Finance: Permission denied (os error 13)
2026-10-23T02:14:34+01:00 run=7f3a2c end    completed in 14m33s — 772,087 found, 771,950 unchanged, 137 written, 12 removed, 22,684 errors (22,683 unreadable, 1 traversal/db), 1,408 duplicate groups, 3.2 GB wasted
```

- **Outcomes:** `completed`, `failed` (with the exit code — fatal errors exit 1), or
  `stopped by user` (`q` in the TUI). A `start` with no `end` means the process was killed.
- **Errors** are capped at 50 per run, then `… and N more errors (not logged)`.
- **`--log FILE`** writes elsewhere; **`--no-log`** turns it off.
- At 10 MB the log is renamed to `<database>.log.1` and a new one started.
- If the log can't be written, the run carries on and prints one warning.

---

## Comparison with earlier versions

| Feature | Python | Rust v1 | Rust v2 | Rust v3 (TUI) | Rust v4 (fixes) | Rust v5 (features) |
|---|---|---|---|---|---|---|
| Hashing | MD5 | BLAKE3 | BLAKE3 | BLAKE3 | BLAKE3 | BLAKE3 |
| Parallelisation | `multiprocessing` | Rayon | Rayon | Rayon | Rayon | Rayon |
| Batch transactions | Yes (100K) | No (single) | Yes (1K) | Yes (1K) | Yes (1K) | Yes (1K) |
| Resumable after crash | No | No | Yes | Yes | Yes | Yes |
| Resume skips already-indexed files | — | — | No | No | Yes | Yes |
| `update` skips unchanged files | — | — | No | No | Yes | Yes |
| `update` catches all changes (not just the last 24 hours) | No | No | No | No | Yes | Yes |
| `update` removes rows for deleted files | No | No | No | No | Yes | Yes |
| One table per directory however it's typed (`/nas`, `/nas/`, `./nas`) | No | No | No | No | Yes | Yes |
| Duplicate detection — scales to large runs, totals cover every group (not just the top 10), includes files from earlier runs; empty files excluded | — | — | — | No | Yes | Yes |
| Hard links not counted as wasted space | — | — | — | No | Yes | Yes |
| Automated test suite (`cargo test`) | No | No | No | No | Yes | Yes |
| Records each indexed path and its last run (`roots` table) | No | No | No | No | Yes (4.1) | Yes |
| `db` maintenance — `info`, `drop`, `vacuum`, `check` | No | No | No | No | Yes (4.1) | Yes |
| Error breakdown (unreadable / not written / traversal) | No | No | No | No | Yes | Yes |
| Network share resilience | Partial | Poor | Yes | Yes | Yes | Yes |
| TUI progress | No | No | No | Yes | Yes | Yes |
| Auto plain-text fallback | — | — | — | Yes | Yes | Yes |
| `--db` flag | No | No | Yes | Yes | Yes | Yes |
| `--threads` flag | No | No | Yes | Yes | Yes | Yes |
| Run log — each command that changes the database, its outcome, a summary and any errors (`<database>.log`) | No | No | No | No | No | Yes (5.0) |
| `search` — query the index by name, size, age, type, duplicates; table / CSV / TSV / JSON / paths | No | No | No | No | No | Yes (5.0) |
| `report` — overview, top files, by type, by folder, stale, duplicates | Scripts | Scripts | Scripts | Scripts | No | Yes (5.1) |
| Duplicate filtering — `--min-size`, `--min-copies`, `--min-wasted`, `--ext`, `--under` | No | No | No | No | No | Yes (5.1) |
| `export` — CSV / TSV / JSON for BI tools | No | No | No | No | No | Yes (5.0) |
| Dry run — preview `index` / `update` without hashing or writing (dedupe previews come with v6) | No | No | No | No | No | Yes (5.2) |
| Include / exclude file types — `--include-ext`, `--exclude-ext`, stored per path | No | No | No | No | No | Yes (5.2) |
| Ignore — exclude paths/patterns from indexing | No | No | No | No | No | Planned (5.3) |
| Jobs — named, saved runs: run by name or pick from a menu, with last-run history | No | No | No | No | No | Planned (5.4) |
| Interactive app — main window plus Jobs / Results / Database / Settings menus | No | No | No | No | No | Planned (5.5) |
| Quarantine — move duplicates to a dated trash folder, with `purge` | No | No | No | No | No | Planned (v6) |
| Backup — rename redundant copies to `.bak` in place | No | No | No | No | No | Planned (v6) |

### Background

The project was developed in multiple languages, each addressing the limitations of the previous:

- **Bash (v1):** Initial proof-of-concept — simple but slow due to sequential per-file processing
- **Bash (v2):** Optimised with parallel `xargs`, deferred index creation, and SQL batching
- **Python:** More structured, with multiprocessing and bulk inserts — but ultimately too slow for large network shares
- **Rust v1:** Best performance, but contained bugs around network share resilience and resumability
- **Rust v2:** All known bugs fixed, plain-text output only
- **Rust v3:** Full TUI with live progress and duplicate detection, with automatic plain-text fallback
- **Rust v4:** Fixes — cheap resume, change detection in `update`, duplicate tracking that scales to large shares. v4.1 adds database maintenance (`diskunder db`)
- **Rust v5** (current, `src/`): Features, in small releases — 5.0 adds `search`, `export` and a run log, 5.1 adds `report`, 5.2 adds `--dry-run` and file-type filters; the rest is planned in [v5-plan.md](v5-plan.md)

Only the current Rust version is kept in this repository; the earlier versions were not committed.

See [changes.md](changes.md) for a detailed change log and [future.md](future.md) for planned features.

---

## Files

```
diskunder/
├── src/               # Rust v5 source
│   ├── main.rs        #   entry point
│   ├── cli.rs         #   command-line arguments
│   ├── scan.rs        #   walking, hashing, the indexing worker
│   ├── db.rs          #   schema, migrations, roots, queries
│   ├── progress.rs    #   run state and live duplicate tracking
│   ├── tui.rs         #   full-screen progress display
│   ├── plain.rs       #   plain-text progress
│   ├── runlog.rs      #   run log
│   ├── query.rs       #   filters and SQL for search/export
│   ├── output.rs      #   table, CSV, TSV, JSON, paths
│   ├── search.rs      #   search and export commands
│   ├── report.rs      #   report command
│   ├── dryrun.rs      #   index / update --dry-run
│   ├── filters.rs     #   file-type filters
│   ├── maintenance.rs #   db info / drop / vacuum / check
│   └── fmt.rs         #   formatting helpers
├── tests/cli.rs       # End-to-end tests (cargo test)
├── diskunder-macos    # Compiled macOS binary (arm64)
├── diskunder-linux    # Compiled Linux x86_64 binary (statically linked, musl)
├── Makefile           # Build targets: make, make mac, make linux, make clean
├── .cargo/config.toml # Cargo cross-compilation config (musl linker)
├── Cargo.toml         # Rust project config
├── Cargo.lock         # Locked dependency versions — ensures reproducible builds
├── changes.md         # Full change log (v1 → v5)
├── future.md          # Feature ideas
└── v5-plan.md         # v5 plan: search, report, export, filters, jobs, interactive app
```

> **Note:** `*.db` files are excluded from version control (`.gitignore`) as they contain
> indexed file paths from your environment. Each user generates their own on first run.

### Building the binaries

Pre-built binaries are included for macOS and Linux x86_64. To rebuild:

| Command | Builds |
|---|---|
| `make` | both `diskunder-macos` and `diskunder-linux` |
| `make mac` | macOS only |
| `make linux` | Linux only |
| `make clean` | removes build artifacts and binaries |

**Prerequisites for Linux cross-compilation (macOS host):**
```bash
brew install FiloSottile/musl-cross/musl-cross
rustup target add x86_64-unknown-linux-musl
```

The Linux binary is statically linked against musl libc — no system dependencies required on the target machine.

### Running the tests

```bash
cargo test
```

[tests/cli.rs](tests/cli.rs) runs the real binary in plain mode against throwaway directories and
checks both its summary output and the database it writes: resume, change detection, deletions,
path normalisation and v3 migration, error counting, duplicate totals, hard links, argument
parsing, the `db` maintenance commands, schema migrations, the run log, `search`/`export`
filters, scopes and formats, every `report` section and duplicate filter, `--dry-run` (checked against the real run that
follows), and file-type filters. Unit tests in `src/query.rs` and `src/filters.rs` cover size,
age and extension parsing and filter matching.
Tests that rely on file permissions skip themselves when run as root.
