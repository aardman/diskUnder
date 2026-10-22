# diskUnder

A command-line tool for indexing file metadata and detecting duplicate files/folders across a filesystem or network share — a CLI equivalent of [DiskOver](https://diskoverdata.com/). File metadata is stored in a local SQLite database, enabling fast queries for duplicates, large files, and directory summaries.

---

## Background

The project was developed in multiple languages, each addressing the limitations of the previous:

- **Bash (v1):** Initial proof-of-concept — simple but slow due to sequential per-file processing
- **Bash (v2):** Optimised with parallel `xargs`, deferred index creation, and SQL batching
- **Python:** More structured, with multiprocessing and bulk inserts — but ultimately too slow for large network shares
- **Rust v1:** Best performance, but contained bugs around network share resilience and resumability
- **Rust v2:** All known bugs fixed, plain-text output only
- **Rust v3:** Full TUI with live progress and duplicate detection, with automatic plain-text fallback
- **Rust v4** (current, `src/main.rs`): Fixes — cheap resume, change detection in `update`, duplicate tracking that scales to large shares. v4.1 adds database maintenance (`diskunder db`)
- **Rust v5+** (planned): Features — see [future.md](future.md)

Only the current Rust version is kept in this repository; the earlier versions were not committed.

See [changes.md](changes.md) for a detailed change log and [future.md](future.md) for planned features.

---

## How It Works

diskUnder walks a target directory, collects file metadata (size, dates, BLAKE3 checksum), and writes it to a SQLite database. Multiple paths can be indexed into the same database — each path gets its own table named `files_<MD5(path)>`, where `path` is the canonical (absolute, symlink-resolved) root. `/nas`, `/nas/`, `./nas` and a symlink to it all share one table, and stored paths are always absolute.

> **Upgrading from v3:** v3 named tables after the path exactly as typed, and stored paths in that spelling (relative if you typed a relative path). The first v4 run with the **same spelling you used before** migrates that table in place — paths are rewritten to absolute and the table renamed — so nothing is re-hashed. If you used several spellings for one directory, only the first one migrated is kept; the others are left untouched.
>
> The `change_date` column is also renamed to `birth_date` on first use (it always held the file's creation time). Update any queries or scripts that select `change_date`.
>
> v4 also adds `device` and `inode` columns. Existing rows get them on the next `update` run without re-hashing; until then, hard links in those rows are counted as duplicates as before.
>
> **Upgrading to v4.1:** the `roots` table is created automatically, and each path is recorded the next time it's indexed or updated. Until then `diskunder db info` lists its table as having no recorded path.

Two modes are supported:

- **`index`** — full traversal of the target path. Files already in the database are skipped without being re-hashed, so an interrupted run can simply be restarted
- **`update`** — full traversal that brings the table in line with disk: new files and files whose size or modified time differ from the database are hashed, unchanged files are skipped without being re-hashed, and rows for files no longer on disk are removed. Rows under a directory that could not be read are kept, and nothing is removed if the walk finds no files at all (e.g. an unmounted share)

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
    last_mode     TEXT NOT NULL       -- index | update
);
```

### SQLite Optimisations

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = 100000;
```

---

## Install

The binaries are self-contained — no runtime dependencies, and the database is created on
first run, so installing is just copying the right one onto your `PATH` as `diskunder`:

```bash
# macOS (Apple Silicon)
sudo mkdir -p /usr/local/bin
sudo install -m 755 diskunder-macos /usr/local/bin/diskunder

# Linux (x86_64 — statically linked, any distribution)
sudo install -m 755 diskunder-linux /usr/local/bin/diskunder

# Or without sudo, for your user only (make sure ~/.local/bin is on your PATH)
mkdir -p ~/.local/bin
install -m 755 diskunder-linux ~/.local/bin/diskunder     # or diskunder-macos

diskunder --version     # → diskunder 0.4.1
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

```bash
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

```bash
# Interactive — full TUI
diskunder index /Volumes/NAS --db /data/index.db --threads 4

# Headless / scripted — auto plain text (progress is written to stderr)
diskunder index /Volumes/NAS --db /data/index.db 2>&1 | tee run.log

# Explicit plain text in an interactive terminal
diskunder --no-tui index /Volumes/NAS --db /data/index.db

# Update mode — pick up new, changed and deleted files
diskunder update /Volumes/NAS --db /data/index.db
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

**Dependencies:** `blake3`, `chrono`, `clap`, `crossterm`, `md5`, `rayon`, `ratatui`, `rusqlite`, `walkdir`

---

## Try it with sample data

A two-minute walkthrough on a throwaway folder — nothing outside `/tmp/diskunder-demo` is
touched. Run it from the repository root. Steps 2–4 use `--no-tui` so your output matches
what's shown here; leave it off to watch the live TUI instead (press `q` when it finishes).

**1. Make some fake data** — three 200 KB "photos", a backup folder holding two real copies
and one hard link, and a small text file:

```bash
BIN=$PWD/diskunder-macos        # on Linux: BIN=$PWD/diskunder-linux  (or BIN=diskunder if installed)
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

**4. Change things and update** — delete a duplicate, edit a file, add a file:

```bash
rm "$DEMO/data/backup/img1.jpg"
echo "milk, eggs" >> "$DEMO/data/notes.txt"
echo "new file" > "$DEMO/data/todo.txt"
$BIN update "$DEMO/data" --db "$DEMO/demo.db" --no-tui
```
```
update /private/tmp/diskunder-demo/data
  2/2 processed — 2 written, 0 errors
complete: 7 files — 5 unchanged, 2 written, 1 removed, 0 errors, 1 duplicate groups
wasted: 195.3 KB
```

Only the edited and new files were hashed, the deleted copy's row was removed, and one
duplicate group remains.

**5. Query the database** (needs the `sqlite3` command — preinstalled on macOS; `apt install sqlite3` on Debian/Ubuntu):

```bash
T=$(sqlite3 "$DEMO/demo.db" "SELECT name FROM sqlite_master WHERE name LIKE 'files_%' AND type='table'")

# Largest files
sqlite3 -header -column "$DEMO/demo.db" \
  "SELECT filename, size, directory FROM $T ORDER BY size DESC LIMIT 3"

# Checksums seen more than once: paths vs distinct copies on disk
sqlite3 -header -column "$DEMO/demo.db" \
  "SELECT substr(checksum,1,12) AS checksum, COUNT(*) AS paths, COUNT(DISTINCT inode) AS copies, size
   FROM $T GROUP BY checksum HAVING COUNT(*) > 1"
```
```
filename  size    directory
--------  ------  ---------------------------------------
img1.jpg  200000  /private/tmp/diskunder-demo/data/photos
img2.jpg  200000  /private/tmp/diskunder-demo/data/photos
img3.jpg  200000  /private/tmp/diskunder-demo/data/photos

checksum      paths  copies  size
------------  -----  ------  ------
7b828503f882  2      2       200000
e22348758b55  2      1       200000
```

The first group is the real duplicate (`img2.jpg` and its backup — 2 copies). The second is
`img3.jpg` and its hard link — 2 paths but only 1 copy, which is why it isn't reported as
wasted space. Your checksums — and so the order of these two rows — will differ, since the files are random.

**6. See what's in the database** — `db info` summarises every indexed path, so you don't need
SQL for the basics:

```bash
$BIN db info --db "$DEMO/demo.db"
```
```
database: /tmp/diskunder-demo/demo.db — 56.0 KB on disk

/private/tmp/diskunder-demo/data
  files       7 (976.6 KB)
  duplicates  1 group, 195.3 KB wasted
  last run    update at 2026-10-22 15:53, finished 2026-10-22 15:53
  first run   2026-10-22 15:53
  table       files_774ac007bc3fea44b72a1c083588ff71 (40.0 KB with indexes)
```

**7. Clean up:**

```bash
rm -rf /tmp/diskunder-demo
```

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

---

## Comparison with earlier versions

| Feature | Python | Rust v1 | Rust v2 | Rust v3 (TUI) | Rust v4 (fixes) | Rust v5+ (planned) |
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
| Duplicate tracking scales to large runs | — | — | — | No | Yes (empty files excluded) | Yes |
| Duplicate totals cover all groups (not just the top 10) | — | — | — | No | Yes | Yes |
| Duplicates found across the whole table (not just files hashed this run) | — | — | — | No | Yes | Yes |
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
| `search` — query the index by name, size, age, type | No | No | No | No | No | Planned |
| `report` — top files/dirs, by type, stale, duplicates | Scripts | Scripts | Scripts | Scripts | No | Planned |
| Duplicate filtering — `--min-size`, `--min-copies`, `--min-wasted`, `--extension` | No | No | No | No | No | Planned |
| `export` — CSV / JSON for BI tools | No | No | No | No | No | Planned |
| Dry run — preview dedupe actions without touching files | No | No | No | No | No | Planned |
| Ignore — exclude paths/patterns from indexing | No | No | No | No | No | Planned |
| Quarantine — move duplicates to a dated trash folder, with `purge` | No | No | No | No | No | Planned |
| Backup — rename redundant copies to `.bak` in place | No | No | No | No | No | Planned |
| Jobs — named, saved runs: run by name or pick from a menu, with last-run history | No | No | No | No | No | Planned |

---

## Files

```
diskunder/
├── src/main.rs        # Rust v4 source
├── tests/cli.rs       # End-to-end tests (cargo test)
├── diskunder-macos    # Compiled macOS binary (arm64)
├── diskunder-linux    # Compiled Linux x86_64 binary (statically linked, musl)
├── Makefile           # Build targets: make, make mac, make linux, make clean
├── .cargo/config.toml # Cargo cross-compilation config (musl linker)
├── Cargo.toml         # Rust project config
├── Cargo.lock         # Locked dependency versions — ensures reproducible builds
├── changes.md         # Full change log (v1 → v4)
└── future.md          # Planned features
```

> **Note:** `*.db` files are excluded from version control (`.gitignore`) as they contain
> indexed file paths from your environment. Each user generates their own on first run.

### Building the binaries

Pre-built binaries are included for macOS and Linux x86_64. To rebuild:

```bash
make        # builds both diskunder-macos and diskunder-linux
make mac    # macOS only
make linux  # Linux only
make clean  # remove build artifacts and binaries
```

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
parsing, and the `db` maintenance commands. Two tests rely on file permissions and skip themselves when run as root.
